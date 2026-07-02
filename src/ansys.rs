use crate::history::{CompactionMode, HistoryFile, build_messages};
use crate::llm::{LlmConfig, call_model};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::json;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_ANSYS_EXE: &str =
    r"F:\ansys\19.0zhuchengxu\ANSYS Inc\v190\ansys\bin\winx64\ANSYS190.exe";
const DEFAULT_INPUT_DIR: &str = "input";
const DEFAULT_OUTPUT_DIR: &str = "output";
const DEFAULT_MAX_REPAIR_ATTEMPTS: usize = 6;
const MAX_SUMMARY_CHARS_PER_TEXT: usize = 4_000;
const MAX_SUMMARY_TEXT_CHARS_TOTAL: usize = 12_000;
const ANSYS_COMPLETION_WAIT_SECS: u64 = 90;
const ANSYS_COMPLETION_POLL_MS: u64 = 1_000;
const MAX_REASONABLE_K_FILE_BYTES: u64 = 250 * 1024 * 1024;
const K_FILE_HEAD_BYTES: usize = 16 * 1024;
const K_FILE_TAIL_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct AnsysConfig {
    pub(crate) executable: PathBuf,
    pub(crate) input_dir: PathBuf,
    pub(crate) output_dir: PathBuf,
    pub(crate) apdl_runs_dir: PathBuf,
    pub(crate) apdl_k_runs_dir: PathBuf,
    pub(crate) k_dir: PathBuf,
    pub(crate) max_repair_attempts: usize,
    pub(crate) stage2_product: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct InputAsset {
    pub(crate) path: String,
    pub(crate) relative_path: String,
    pub(crate) kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct RunSummary {
    pub(crate) run_id: String,
    pub(crate) run_dir: PathBuf,
    pub(crate) apdl_runs_dir: PathBuf,
    pub(crate) apdl_k_runs_dir: PathBuf,
    pub(crate) k_dir: PathBuf,
    pub(crate) source_context_path: PathBuf,
    pub(crate) input_summary_path: PathBuf,
    pub(crate) draft_apdl_path: PathBuf,
    pub(crate) err_file: Option<PathBuf>,
    pub(crate) out_file: PathBuf,
    pub(crate) k_file: Option<PathBuf>,
    pub(crate) stage2_dir: PathBuf,
    pub(crate) stage2_current_apdl_path: PathBuf,
    pub(crate) stage2_brief_path: PathBuf,
    pub(crate) stage2_request_path: PathBuf,
    pub(crate) stage2_run_dir: Option<PathBuf>,
    pub(crate) stage2_out_file: Option<PathBuf>,
    pub(crate) stage2_err_file: Option<PathBuf>,
    pub(crate) stage2_attempts: usize,
    pub(crate) attempts: usize,
    pub(crate) message: String,
}

#[derive(Debug)]
struct AttemptOutcome {
    draft_path: PathBuf,
    out_file: PathBuf,
    err_file: Option<PathBuf>,
    err_text: Option<String>,
    stderr_text: Option<String>,
    k_file: Option<PathBuf>,
}

#[derive(Debug)]
struct Stage2Outcome {
    run_dir: PathBuf,
    out_file: PathBuf,
    err_file: Option<PathBuf>,
    k_file: Option<PathBuf>,
    attempts: usize,
}

pub(crate) fn load_config(workspace_root: &Path) -> AnsysConfig {
    let executable = env_path("OPENANSYS_ANSYS_EXE")
        .or_else(|| env_path("ANSYS_EXE_PATH"))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ANSYS_EXE));
    let stage2_product = std::env::var("OPENANSYS_STAGE2_PRODUCT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "dyna".to_string());
    let input_dir = env_path("OPENANSYS_INPUT_DIR")
        .unwrap_or_else(|| workspace_root.join(DEFAULT_INPUT_DIR));
    let output_dir = env_path("OPENANSYS_OUTPUT_DIR")
        .unwrap_or_else(|| workspace_root.join(DEFAULT_OUTPUT_DIR));
    let apdl_runs_dir = output_dir.join("apdl");
    let apdl_k_runs_dir = output_dir.join("apdl-k");
    let k_dir = output_dir.join("k");
    let max_repair_attempts = std::env::var("OPENANSYS_MAX_REPAIR_ATTEMPTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_REPAIR_ATTEMPTS);

    AnsysConfig {
        executable,
        input_dir,
        output_dir,
        apdl_runs_dir,
        apdl_k_runs_dir,
        k_dir,
        max_repair_attempts,
        stage2_product,
    }
}

pub(crate) fn prepare_input_bundle(config: &AnsysConfig) -> Result<(Vec<InputAsset>, PathBuf, PathBuf)> {
    fs::create_dir_all(&config.output_dir)
        .with_context(|| format!("failed to create {}", config.output_dir.display()))?;
    fs::create_dir_all(&config.apdl_runs_dir)
        .with_context(|| format!("failed to create {}", config.apdl_runs_dir.display()))?;
    fs::create_dir_all(&config.apdl_k_runs_dir)
        .with_context(|| format!("failed to create {}", config.apdl_k_runs_dir.display()))?;
    fs::create_dir_all(&config.k_dir)
        .with_context(|| format!("failed to create {}", config.k_dir.display()))?;

    let assets = scan_input_dir(&config.input_dir)?;
    if assets.is_empty() {
        bail!("no supported input assets found under {}", config.input_dir.display());
    }

    let source_context_path = config.output_dir.join("source_context.json");
    let input_summary_path = config.output_dir.join("input_summary.txt");
    let input_summary = build_input_summary(&assets);

    fs::write(&source_context_path, serde_json::to_string_pretty(&assets)?)
        .with_context(|| format!("failed to write {}", source_context_path.display()))?;
    fs::write(&input_summary_path, input_summary)
        .with_context(|| format!("failed to write {}", input_summary_path.display()))?;

    Ok((assets, source_context_path, input_summary_path))
}

pub(crate) fn run_pipeline(
    client: &reqwest::blocking::Client,
    llm: &LlmConfig,
    workspace_root: &Path,
    config: &AnsysConfig,
    history_token_limit: u64,
) -> Result<RunSummary> {
    let (assets, source_context_path, input_summary_path) = prepare_input_bundle(config)?;
    let input_summary = fs::read_to_string(&input_summary_path)
        .with_context(|| format!("failed to read {}", input_summary_path.display()))?;

    let run_id = format!("run-{}", unix_seconds());
    let mut current_apdl = generate_initial_apdl(
        client,
        llm,
        workspace_root,
        &assets,
        &input_summary,
        config,
    )?;
    let mut repair_notes = Vec::new();
    let repair_history_path = config
        .apdl_runs_dir
        .join(format!("{run_id}-repair-history.json"));
    let mut repair_agent = RepairAgent::new(workspace_root, repair_history_path, history_token_limit);

    for attempt in 1..=config.max_repair_attempts {
        let attempt_name = format!("{run_id}-attempt-{attempt:02}");
        let run_dir = config.apdl_runs_dir.join(&attempt_name);
        fs::create_dir_all(&run_dir)
            .with_context(|| format!("failed to create {}", run_dir.display()))?;

        let outcome = execute_ansys(&config.executable, None, config, &run_dir, &attempt_name, &current_apdl)?;
        if let Some(issue_text) = outcome
            .err_text
            .as_ref()
            .cloned()
            .or_else(|| validate_stage1_model_requirements(&current_apdl))
        {
            let repair_prompt = build_repair_prompt(
                workspace_root,
                config,
                &outcome.draft_path,
                &current_apdl,
                &issue_text,
                &repair_notes,
                outcome.stderr_text.as_deref(),
            );
            let repair_prompt_path = run_dir.join("repair_prompt.txt");
            fs::write(&repair_prompt_path, &repair_prompt)
                .with_context(|| format!("failed to write {}", repair_prompt_path.display()))?;

            let repaired = repair_agent.repair(client, llm, repair_prompt)?;
            let repaired_path = run_dir.join("repaired.apdl");
            fs::write(&repaired_path, &repaired)
                .with_context(|| format!("failed to write {}", repaired_path.display()))?;
            let issue_source = if outcome.err_text.is_some() {
                outcome
                    .err_file
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "ANSYS output".to_string())
            } else {
                "stage-1 completeness validation".to_string()
            };
            repair_notes.push(format!(
                "Attempt {attempt}: repaired APDL based on {}",
                issue_source
            ));
            current_apdl = repaired;
            continue;
        }

        let k_file = outcome.k_file.or_else(|| newest_k_file(&config.k_dir).ok().flatten());
        let stage2_dir = config
            .apdl_k_runs_dir
            .join(format!("{run_id}-stage2"));
        fs::create_dir_all(&stage2_dir)
            .with_context(|| format!("failed to create {}", stage2_dir.display()))?;
        let stage2_current_apdl_path = stage2_dir.join("current_stage2.apdl");
        if !stage2_current_apdl_path.exists() {
            let seed_stage2_apdl = build_stage2_seed_apdl(&outcome.draft_path, &stage2_dir);
            fs::write(&stage2_current_apdl_path, seed_stage2_apdl).with_context(|| {
                format!("failed to write {}", stage2_current_apdl_path.display())
            })?;
        }
        let stage2_brief_path = stage2_dir.join("stage2_export_brief.txt");
        let stage2_request_path = stage2_dir.join("stage2_export_request.txt");
        let stage2_brief = build_stage2_export_brief(
            workspace_root,
            config,
            &run_id,
            &run_dir,
            &outcome.draft_path,
            outcome.out_file.as_path(),
            outcome.err_file.as_deref(),
            k_file.as_deref(),
        );
        fs::write(&stage2_brief_path, stage2_brief)
            .with_context(|| format!("failed to write {}", stage2_brief_path.display()))?;
        let stage2_request = build_stage2_export_request(
            workspace_root,
            config,
            &run_id,
            &run_dir,
            &outcome.draft_path,
            outcome.out_file.as_path(),
            outcome.err_file.as_deref(),
            &stage2_dir,
        );
        fs::write(&stage2_request_path, stage2_request)
            .with_context(|| format!("failed to write {}", stage2_request_path.display()))?;
        let stage2_outcome = run_stage2_pipeline(
            client,
            llm,
            workspace_root,
            config,
            history_token_limit,
            &run_id,
            &run_dir,
            &outcome.draft_path,
            outcome.out_file.as_path(),
            outcome.err_file.as_deref(),
            &stage2_dir,
            &stage2_current_apdl_path,
            &stage2_request_path,
        )?;
        let final_k_file = stage2_outcome.k_file.clone().or(k_file);
        let success_message = if final_k_file.is_some() {
            "ANSYS run completed without blocking .err issues, satisfied Stage 1 geometry/mesh checks, and Stage 2 produced a K file.".to_string()
        } else {
            "ANSYS run completed without blocking .err issues and satisfied Stage 1 geometry/mesh checks. Stage 2 executed but no K file was produced yet.".to_string()
        };
        return Ok(RunSummary {
            run_id,
            run_dir,
            apdl_runs_dir: config.apdl_runs_dir.clone(),
            apdl_k_runs_dir: config.apdl_k_runs_dir.clone(),
            k_dir: config.k_dir.clone(),
            source_context_path,
            input_summary_path,
            draft_apdl_path: outcome.draft_path,
            err_file: outcome.err_file,
            out_file: outcome.out_file,
            k_file: final_k_file,
            stage2_dir,
            stage2_current_apdl_path,
            stage2_brief_path,
            stage2_request_path,
            stage2_run_dir: Some(stage2_outcome.run_dir),
            stage2_out_file: Some(stage2_outcome.out_file),
            stage2_err_file: stage2_outcome.err_file,
            stage2_attempts: stage2_outcome.attempts,
            attempts: attempt,
            message: success_message,
        });
    }

    Err(anyhow!(
        "exhausted {} repair attempts without reaching an error-free ANSYS run",
        config.max_repair_attempts
    ))
}

pub(crate) fn run_stage2_only(
    client: &reqwest::blocking::Client,
    llm: &LlmConfig,
    workspace_root: &Path,
    config: &AnsysConfig,
    history_token_limit: u64,
) -> Result<RunSummary> {
    let stage2_dir = latest_stage2_workspace(config)?
        .ok_or_else(|| anyhow!("no stage2 workspace found under {}", config.apdl_k_runs_dir.display()))?;
    let stage2_current_apdl_path = stage2_dir.join("current_stage2.apdl");
    let stage2_request_path = stage2_dir.join("stage2_export_request.txt");
    let stage2_brief_path = stage2_dir.join("stage2_export_brief.txt");
    bootstrap_existing_stage2_workspace(config, &stage2_dir, &stage2_current_apdl_path, &stage2_request_path, &stage2_brief_path)?;
    let current_apdl = fs::read_to_string(&stage2_current_apdl_path)
        .with_context(|| format!("failed to read {}", stage2_current_apdl_path.display()))?;
    if stage2_apdl_needs_generation(&current_apdl) {
        bail!(
            "stage2 current apdl is still a placeholder: {}. Replace it with the APDL you want to run, or use /run first.",
            stage2_current_apdl_path.display()
        );
    }

    let run_id = stage2_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("stage2")
        .to_string();
    let repair_history_path = stage2_dir.join(format!("{run_id}-manual-stage2-repair-history.json"));
    let manual_dir = config.output_dir.join("manual").join(&run_id);
    fs::create_dir_all(&manual_dir)
        .with_context(|| format!("failed to create {}", manual_dir.display()))?;
    let mut repair_agent = RepairAgent::new(workspace_root, repair_history_path, history_token_limit);
    let mut repair_notes = Vec::new();
    let mut working_apdl = current_apdl;
    for attempt in 1..=config.max_repair_attempts {
        if let Some(issue_text) = validate_stage2_apdl_strategy(&working_apdl) {
            let repair_prompt = build_stage2_repair_prompt(
                workspace_root,
                config,
                &stage2_dir,
                &stage2_current_apdl_path,
                &stage2_request_path,
                None,
                &stage2_current_apdl_path,
                &working_apdl,
                &issue_text,
                &repair_notes,
                None,
            );
            let repaired = repair_agent.repair(client, llm, repair_prompt)?;
            fs::write(&stage2_current_apdl_path, &repaired)
                .with_context(|| format!("failed to update {}", stage2_current_apdl_path.display()))?;
            repair_notes.push(format!(
                "Manual Stage 2 attempt {attempt}: repaired APDL based on stage-2 strategy validation"
            ));
            working_apdl = repaired;
            continue;
        }
        let attempt_name = format!("{run_id}-manual-stage2-attempt-{attempt:02}");
        let run_dir = manual_dir.join(format!("manual-attempt-{attempt:02}"));
        fs::create_dir_all(&run_dir)
            .with_context(|| format!("failed to create {}", run_dir.display()))?;
        let outcome = execute_ansys(
            &config.executable,
            Some(&config.stage2_product),
            config,
            &run_dir,
            &attempt_name,
            &working_apdl,
        )?;
        let issue_text = outcome
            .err_text
            .as_ref()
            .cloned()
            .or_else(|| validate_stage2_apdl_strategy(&working_apdl))
            .or_else(|| {
                validate_stage2_export_requirements(
                    outcome.k_file.as_deref(),
                    outcome.out_file.as_path(),
                    config,
                )
            });

        let final_run_dir = run_dir.clone();
        let final_out_file = outcome.out_file.clone();
        let final_err_file = outcome.err_file.clone();
        let final_k_file = outcome.k_file.clone().or_else(|| newest_k_file(&config.k_dir).ok().flatten());

        if let Some(issue_text) = issue_text {
            let repair_prompt = build_stage2_repair_prompt(
                workspace_root,
                config,
                &stage2_dir,
                &stage2_current_apdl_path,
                &stage2_request_path,
                None,
                &outcome.draft_path,
                &working_apdl,
                &issue_text,
                &repair_notes,
                outcome.stderr_text.as_deref(),
            );
            let repair_prompt_path = run_dir.join("repair_prompt.txt");
            fs::write(&repair_prompt_path, &repair_prompt)
                .with_context(|| format!("failed to write {}", repair_prompt_path.display()))?;
            let repaired = repair_agent.repair(client, llm, repair_prompt)?;
            let repaired_path = run_dir.join("repaired.apdl");
            fs::write(&repaired_path, &repaired)
                .with_context(|| format!("failed to write {}", repaired_path.display()))?;
            fs::write(&stage2_current_apdl_path, &repaired)
                .with_context(|| format!("failed to update {}", stage2_current_apdl_path.display()))?;
            repair_notes.push(format!(
                "Manual Stage 2 attempt {attempt}: repaired APDL based on {}",
                if outcome.err_text.is_some() {
                    outcome
                        .err_file
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "ANSYS output".to_string())
                } else {
                    "stage-2 K export validation".to_string()
                }
            ));
            working_apdl = repaired;
            continue;
        }

        return Ok(RunSummary {
            run_id,
            run_dir: final_run_dir.clone(),
            apdl_runs_dir: config.apdl_runs_dir.clone(),
            apdl_k_runs_dir: config.apdl_k_runs_dir.clone(),
            k_dir: config.k_dir.clone(),
            source_context_path: config.output_dir.join("source_context.json"),
            input_summary_path: config.output_dir.join("input_summary.txt"),
            draft_apdl_path: stage2_current_apdl_path.clone(),
            err_file: None,
            out_file: final_out_file.clone(),
            k_file: final_k_file,
            stage2_dir,
            stage2_current_apdl_path,
            stage2_brief_path,
            stage2_request_path,
            stage2_run_dir: Some(final_run_dir),
            stage2_out_file: Some(final_out_file),
            stage2_err_file: final_err_file,
            stage2_attempts: attempt,
            attempts: 0,
            message: "Stage 2 executed directly from apdl-k and produced a K file.".to_string(),
        });
    }

    Err(anyhow!(
        "exhausted {} Stage 2 repair attempts without producing a K file",
        config.max_repair_attempts
    ))
}

fn generate_initial_apdl(
    client: &reqwest::blocking::Client,
    llm: &LlmConfig,
    workspace_root: &Path,
    assets: &[InputAsset],
    input_summary: &str,
    config: &AnsysConfig,
) -> Result<String> {
    let image_notes = assets
        .iter()
        .filter(|asset| asset.kind == "image")
        .map(|asset| format!("- image file available: {}", asset.relative_path))
        .collect::<Vec<_>>()
        .join("\n");

    let mut messages = vec![json!({
        "role": "system",
        "content": "You are OpenAnsys Agent. Generate complete ANSYS 19.0 APDL command streams for Windows. The current goal is Stage 1 only: build the model, complete the mesh, and avoid ANSYS .err errors or geometry-breaking warnings. Prefer an APDL structure that is suitable for explicit dynamics and later LS-DYNA export. Stage 1 should prepare geometry, support conditions, and mesh foundations for a later collapse workflow, but does not need to export the K file yet. Return only raw APDL text. Do not use markdown fences. Do not explain."
    })];
    messages.push(json!({
        "role": "user",
        "content": format!(
            concat!(
                "Workspace root: {}\n",
                "ANSYS executable: {}\n",
                "Input directory: {}\n",
                "Output APDL directory: {}\n\n",
                "Input summary follows:\n{}\n\n",
                "Additional image assets:\n{}\n\n",
                "Requirements:\n",
                "- Generate APDL for ANSYS 19.0 on Windows.\n",
                "- Build the model from the provided materials.\n",
                "- Prioritize a stable Stage 1 model build with no blocking warnings.\n",
                "- This Stage 1 output must produce a connected, meshed model that can later be adapted for explicit dynamics / LS-DYNA export.\n",
                "- Treat this as an explicit-dynamics-oriented bridge model that will later be used for collapse behavior after support loss.\n",
                "- Do not rely on placeholder entity ids such as _RETURN for area/volume creation.\n",
                "- If you extrude areas into volumes, capture or select the created entities explicitly so geometry commands are not ignored.\n",
                "- The final geometry must remain connected: deck, inclined legs, piers, and foundations must all be present and attached.\n",
                "- Every structural connection must be modeled with shared, coplanar interfaces; do not leave gaps, hanging offsets, visually separated seams, or disconnected touching-only geometry between adjacent solids.\n",
                "- Adjacent parts that are intended to be rigidly connected must share topology after modeling, using coincident interface geometry and glue/merge operations so the final mesh can share nodes across those interfaces.\n",
                "- Add a rigid bottom support surface or rigid ground foundation under the bridge so the four legs are initially seated against that ground geometry.\n",
                "- Define element type(s), material assignment, mesh sizing, and actual mesh generation commands in the APDL.\n",
                "- Prefer an explicit-friendly solid element strategy when it is supported by this ANSYS installation.\n",
                "- The final mesh must use hexahedral elements only.\n",
                "- If any region is not directly meshable with hexahedra, modify or partition the geometry until the full model can be meshed with mapped or sweep hexahedral elements only.\n",
                "- Refine the mesh at connections when appropriate, especially at leg-deck, leg-base, pier-deck, and other structural transition or load-transfer zones.\n",
                "- Keep the global mesh reasonably fine and make the connection regions finer when the geometry suggests higher stress concentration or contact sensitivity.\n",
                "- Do not stop after saving geometry only; the script must reach a meshed model state.\n",
                "- Use conservative, explicit APDL.\n",
                "- Return only the full APDL text.\n"
            ),
            workspace_root.display(),
            config.executable.display(),
            config.input_dir.display(),
            config.apdl_runs_dir.display(),
            input_summary,
            if image_notes.is_empty() { "(none)".to_string() } else { image_notes },
        )
    }));

    let reply = call_model(client, llm, messages, false)?;
    let apdl = strip_code_fences(&reply.content);
    if apdl.trim().is_empty() {
        bail!("model returned empty APDL for initial generation");
    }
    Ok(apdl)
}

fn execute_ansys(
    executable: &Path,
    product: Option<&str>,
    config: &AnsysConfig,
    run_dir: &Path,
    job_name: &str,
    apdl_text: &str,
) -> Result<AttemptOutcome> {
    if !executable.exists() {
        bail!("ANSYS executable not found: {}", executable.display());
    }

    let draft_path = run_dir.join("draft.apdl");
    fs::write(&draft_path, apdl_text)
        .with_context(|| format!("failed to write {}", draft_path.display()))?;

    let out_file = run_dir.join("ansys.out");
    let stderr_file = run_dir.join("ansys.stderr.txt");
    let mut command = Command::new(executable);
    command
        .arg("-b")
        .arg("-dir")
        .arg(run_dir)
        .arg("-j")
        .arg(job_name)
        .arg("-i")
        .arg(&draft_path)
        .arg("-o")
        .arg(&out_file);
    if let Some(product) = product.filter(|value| !value.trim().is_empty()) {
        command.arg("-p").arg(product);
    }
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to launch {}", executable.display()))?;

    let stderr_text = if output.stderr.is_empty() {
        None
    } else {
        let text = String::from_utf8_lossy(&output.stderr).into_owned();
        fs::write(&stderr_file, &text)
            .with_context(|| format!("failed to write {}", stderr_file.display()))?;
        Some(text)
    };

    let err_candidate = run_dir.join(format!("{job_name}.err"));
    let mut err_text = if err_candidate.exists() {
        let text = fs::read_to_string(&err_candidate)
            .with_context(|| format!("failed to read {}", err_candidate.display()))?;
        classify_ansys_run_issue(&text)
    } else {
        None
    };

    if let Some(issue) = wait_for_ansys_completion(&out_file) {
        match &mut err_text {
            Some(existing) => {
                existing.push_str("\n\n");
                existing.push_str(&issue);
            }
            None => err_text = Some(issue),
        }
    }

    Ok(AttemptOutcome {
        draft_path,
        out_file,
        err_file: if err_candidate.exists() { Some(err_candidate) } else { None },
        err_text,
        stderr_text,
        k_file: newest_k_file(&config.k_dir)?,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_stage2_pipeline(
    client: &reqwest::blocking::Client,
    llm: &LlmConfig,
    workspace_root: &Path,
    config: &AnsysConfig,
    history_token_limit: u64,
    run_id: &str,
    stage1_run_dir: &Path,
    stage1_draft_path: &Path,
    stage1_out_file: &Path,
    stage1_err_file: Option<&Path>,
    stage2_dir: &Path,
    stage2_current_apdl_path: &Path,
    stage2_request_path: &Path,
) -> Result<Stage2Outcome> {
    let request_text = fs::read_to_string(stage2_request_path)
        .with_context(|| format!("failed to read {}", stage2_request_path.display()))?;
    let mut current_apdl = ensure_stage2_current_apdl(
        client,
        llm,
        workspace_root,
        config,
        stage1_run_dir,
        stage1_draft_path,
        stage1_out_file,
        stage1_err_file,
        stage2_dir,
        stage2_current_apdl_path,
        &request_text,
    )?;

    let repair_history_path = stage2_dir.join(format!("{run_id}-stage2-repair-history.json"));
    let mut repair_agent = RepairAgent::new(workspace_root, repair_history_path, history_token_limit);
    let mut repair_notes = Vec::new();

    for attempt in 1..=config.max_repair_attempts {
        if let Some(issue_text) = validate_stage2_apdl_strategy(&current_apdl) {
            let repair_prompt = build_stage2_repair_prompt(
                workspace_root,
                config,
                stage1_run_dir,
                stage1_draft_path,
                stage1_out_file,
                stage1_err_file,
                stage2_current_apdl_path,
                &current_apdl,
                &issue_text,
                &repair_notes,
                None,
            );
            let repaired = repair_agent.repair(client, llm, repair_prompt)?;
            fs::write(stage2_current_apdl_path, &repaired).with_context(|| {
                format!("failed to update {}", stage2_current_apdl_path.display())
            })?;
            repair_notes.push(format!(
                "Stage 2 attempt {attempt}: repaired APDL based on stage-2 strategy validation"
            ));
            current_apdl = repaired;
            continue;
        }
        let attempt_name = format!("{run_id}-stage2-attempt-{attempt:02}");
        let run_dir = stage2_dir.join(format!("attempt-{attempt:02}"));
        fs::create_dir_all(&run_dir)
            .with_context(|| format!("failed to create {}", run_dir.display()))?;

        let outcome = execute_ansys(
            &config.executable,
            Some(&config.stage2_product),
            config,
            &run_dir,
            &attempt_name,
            &current_apdl,
        )?;
        let issue_text = outcome
            .err_text
            .as_ref()
            .cloned()
            .or_else(|| validate_stage2_apdl_strategy(&current_apdl))
            .or_else(|| {
                validate_stage2_export_requirements(
                    outcome.k_file.as_deref(),
                    outcome.out_file.as_path(),
                    config,
                )
            });

        if let Some(issue_text) = issue_text {
            let repair_prompt = build_stage2_repair_prompt(
                workspace_root,
                config,
                stage1_run_dir,
                stage1_draft_path,
                stage1_out_file,
                stage1_err_file,
                &outcome.draft_path,
                &current_apdl,
                &issue_text,
                &repair_notes,
                outcome.stderr_text.as_deref(),
            );
            let repair_prompt_path = run_dir.join("repair_prompt.txt");
            fs::write(&repair_prompt_path, &repair_prompt)
                .with_context(|| format!("failed to write {}", repair_prompt_path.display()))?;

            let repaired = repair_agent.repair(client, llm, repair_prompt)?;
            let repaired_path = run_dir.join("repaired.apdl");
            fs::write(&repaired_path, &repaired)
                .with_context(|| format!("failed to write {}", repaired_path.display()))?;
            fs::write(stage2_current_apdl_path, &repaired).with_context(|| {
                format!("failed to update {}", stage2_current_apdl_path.display())
            })?;
            let issue_source = if outcome.err_text.is_some() {
                outcome
                    .err_file
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "ANSYS output".to_string())
            } else {
                "stage-2 K export validation".to_string()
            };
            repair_notes.push(format!(
                "Stage 2 attempt {attempt}: repaired APDL based on {}",
                issue_source
            ));
            current_apdl = repaired;
            continue;
        }

        return Ok(Stage2Outcome {
            run_dir,
            out_file: outcome.out_file,
            err_file: outcome.err_file,
            k_file: outcome.k_file,
            attempts: attempt,
        });
    }

    Err(anyhow!(
        "exhausted {} Stage 2 repair attempts without producing a K file",
        config.max_repair_attempts
    ))
}

#[allow(clippy::too_many_arguments)]
fn ensure_stage2_current_apdl(
    client: &reqwest::blocking::Client,
    llm: &LlmConfig,
    workspace_root: &Path,
    config: &AnsysConfig,
    stage1_run_dir: &Path,
    stage1_draft_path: &Path,
    stage1_out_file: &Path,
    stage1_err_file: Option<&Path>,
    stage2_dir: &Path,
    stage2_current_apdl_path: &Path,
    request_text: &str,
) -> Result<String> {
    let existing = fs::read_to_string(stage2_current_apdl_path)
        .with_context(|| format!("failed to read {}", stage2_current_apdl_path.display()))?;
    if !stage2_apdl_needs_generation(&existing) {
        return Ok(existing);
    }

    let generated = generate_stage2_initial_apdl(
        client,
        llm,
        workspace_root,
        config,
        stage1_run_dir,
        stage1_draft_path,
        stage1_out_file,
        stage1_err_file,
        stage2_dir,
        request_text,
    )?;
    fs::write(stage2_current_apdl_path, &generated)
        .with_context(|| format!("failed to write {}", stage2_current_apdl_path.display()))?;
    Ok(generated)
}

fn stage2_apdl_needs_generation(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty()
        || trimmed.contains("Placeholder only for now")
        || trimmed.starts_with("Tool:")
        || trimmed.contains("<shell_tool>")
        || trimmed.contains("\"command\":")
        || !trimmed.lines().any(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with('!')
        })
}

#[allow(clippy::too_many_arguments)]
fn generate_stage2_initial_apdl(
    client: &reqwest::blocking::Client,
    llm: &LlmConfig,
    workspace_root: &Path,
    config: &AnsysConfig,
    stage1_run_dir: &Path,
    stage1_draft_path: &Path,
    stage1_out_file: &Path,
    stage1_err_file: Option<&Path>,
    stage2_dir: &Path,
    request_text: &str,
) -> Result<String> {
    let stage1_apdl = fs::read_to_string(stage1_draft_path)
        .with_context(|| format!("failed to read {}", stage1_draft_path.display()))?;
    let stage1_out = fs::read_to_string(stage1_out_file)
        .with_context(|| format!("failed to read {}", stage1_out_file.display()))?;
    let stage1_out_excerpt = truncate_chars(stage1_out.trim(), 12_000);
    let err_text = match stage1_err_file {
        Some(path) => fs::read_to_string(path).unwrap_or_else(|_| "(failed to read stage1 err)".to_string()),
        None => "(none)".to_string(),
    };

    let messages = vec![
        json!({
            "role": "system",
            "content": "You are OpenAnsys Agent. Generate Stage 2 ANSYS 19.0 APDL focused on producing an LS-DYNA-compatible .K file from a successful Stage 1 model. Reuse Stage 1 artifacts when possible. This is an explicit dynamics setup intended for later collapse behavior after support loss in LS-PrePost / LS-DYNA style workflows. The .K file must be generated by ANSYS-side commands/workflow, not handwritten as a giant keyword text dump. Do not emit APDL that loops over every node and element with *CFOPEN/*CFWRITE/*VWRITE to author the final deck text. Return only raw APDL text. Do not use markdown fences. Do not explain."
        }),
        json!({
            "role": "user",
            "content": format!(
                concat!(
                    "Workspace root: {}\n",
                    "Stage 2 ANSYS executable: {}\n",
                    "Stage 2 product mode: {}\n",
                    "Stage 1 run directory: {}\n",
                    "Stage 2 workspace: {}\n",
                    "Target K directory: {}\n\n",
                    "Stage 2 request:\n{}\n\n",
                    "Stage 1 APDL:\n{}\n\n",
                    "Stage 1 ansys.out excerpt:\n{}\n\n",
                    "Stage 1 err content:\n{}\n\n",
                    "Requirements:\n",
                    "- Prefer resuming or reusing the successful Stage 1 model/database.\n",
                    "- Focus only on APDL or ANSYS 19.0 commands needed to produce a .K file.\n",
                    "- Prefer the LS-DYNA product/session path and PR_DYNA-style product selection over generic MAPDL defaults when export capability depends on session entry.\n",
                    "- Treat this as an explicit-dynamics model that must later support support-loss / collapse behavior in LS-PrePost or LS-DYNA post workflows.\n",
                    "- Add gravity loading in the Stage 2 setup.\n",
                    "- Treat the bottom ground/support surface as a rigid body or rigid support side of the model where appropriate for the ANSYS-to-LS-DYNA workflow.\n",
                    "- Preserve the initial seated contact between the four bridge legs and the bottom support/ground.\n",
                    "- Add the contact / boundary / explicit-analysis setup needed so later removal of support can produce physically meaningful collapse of unsupported portions.\n",
                    "- The .K file should be generated by ANSYS itself during the APDL-driven workflow.\n",
                    "- Write the .K output into the configured k directory.\n",
                    "- Do not manually author the final keyword deck with *CFOPEN/*CFWRITE/*VWRITE loops over all nodes and elements.\n",
                    "- Keep this script dedicated to Stage 2 export work.\n",
                    "- Return only the complete APDL text.\n"
                ),
                workspace_root.display(),
                config.executable.display(),
                config.stage2_product,
                stage1_run_dir.display(),
                stage2_dir.display(),
                config.k_dir.display(),
                request_text,
                stage1_apdl,
                stage1_out_excerpt,
                err_text,
            )
        }),
    ];

    let reply = call_model(client, llm, messages, false)?;
    let apdl = strip_code_fences(&reply.content);
    if apdl.trim().is_empty() {
        bail!("model returned empty APDL for Stage 2 generation");
    }
    Ok(apdl)
}

fn build_repair_prompt(
    workspace_root: &Path,
    config: &AnsysConfig,
    draft_path: &Path,
    current_apdl: &str,
    err_text: &str,
    repair_notes: &[String],
    stderr_text: Option<&str>,
) -> String {
    let mut prompt = format!(
        concat!(
            "Repair this ANSYS 19.0 APDL command stream.\n",
            "You are only responsible for fixing the ANSYS run issues.\n",
            "Current goal: Stage 1 model build without ANSYS .err errors, without geometry-breaking warnings, and with completed element/mesh setup.\n",
            "Workspace root: {}\n",
            "Stage 2 ANSYS executable: {}\n",
            "Stage 2 product mode: {}\n",
            "Current draft file: {}\n\n",
            "Recent repair history:\n{}\n\n",
            "Current APDL:\n{}\n\n",
            "Issue report derived from the full .err content:\n{}\n\n",
            "Return the full corrected APDL text only.\n",
            "Preserve the modeling intent.\n",
            "No tools are available to you in this repair step. Do not ask to inspect files, do not emit tool calls, and do not describe what you plan to do.\n",
            "If warnings show ignored or invalid geometry commands, treat them as blocking and repair the APDL so all required structural parts are created and connected.\n",
            "If the issue report says Stage 1 is incomplete, add the missing element definition, material assignment, mesh sizing, and mesh generation steps.\n",
            "Preserve or restore the explicit-dynamics-oriented modeling intent: include a rigid ground/bottom support geometry, keep the four bridge legs seated to that base geometry, prefer hexahedral mesh where feasible, allow tetrahedral fallback only where necessary, and refine the mesh at key connection regions when appropriate.\n"
        ),
        workspace_root.display(),
        config.executable.display(),
        config.stage2_product,
        draft_path.display(),
        if repair_notes.is_empty() {
            "(none yet)".to_string()
        } else {
            repair_notes.join("\n")
        },
        current_apdl,
        err_text
    );
    if let Some(stderr_text) = stderr_text.filter(|text| !text.trim().is_empty()) {
        prompt.push_str("\nSupplemental stderr content:\n");
        prompt.push_str(stderr_text);
        prompt.push('\n');
    }
    if prompt.len() > 200_000 {
        prompt.truncate(200_000);
        prompt.push_str("\n\n[repair prompt truncated]");
    }
    prompt
}

#[allow(clippy::too_many_arguments)]
fn build_stage2_repair_prompt(
    workspace_root: &Path,
    config: &AnsysConfig,
    stage1_run_dir: &Path,
    stage1_draft_path: &Path,
    stage1_out_file: &Path,
    stage1_err_file: Option<&Path>,
    draft_path: &Path,
    current_apdl: &str,
    issue_text: &str,
    repair_notes: &[String],
    stderr_text: Option<&str>,
) -> String {
    let mut prompt = format!(
        concat!(
            "Repair this ANSYS 19.0 Stage 2 APDL command stream.\n",
            "You are only responsible for getting the LS-DYNA / .K export step to succeed.\n",
            "Current goal: starting from a successful Stage 1 model, make ANSYS itself generate the .K file without ANSYS .err errors or blocking warnings.\n",
            "Workspace root: {}\n",
            "Stage 2 ANSYS executable: {}\n",
            "Stage 2 product mode: {}\n",
            "Stage 1 run directory: {}\n",
            "Stage 1 APDL: {}\n",
            "Stage 1 ansys.out: {}\n",
            "Stage 1 err file: {}\n",
            "Current Stage 2 draft file: {}\n",
            "Target k directory: {}\n\n",
            "Recent repair history:\n{}\n\n",
            "Current Stage 2 APDL:\n{}\n\n",
            "Stage 2 issue report:\n{}\n\n",
            "Return the full corrected Stage 2 APDL text only.\n",
            "Preserve the successful Stage 1 model handoff and focus on export commands, resume/database usage, product switches, PR_DYNA-style product selection, and ANSYS-side .K generation.\n",
            "No tools are available to you in this repair step. Do not ask to inspect files, do not emit tool calls, and do not describe what you plan to do.\n",
            "Do not manually author the final .K deck text with *CFOPEN/*CFWRITE/*VWRITE loops over all nodes and elements.\n",
            "The corrected APDL must drive an ANSYS-side export/generation workflow, not a handwritten keyword dump.\n",
            "Preserve the explicit-dynamics intent: include gravity, preserve or define the rigid bottom support/ground behavior, keep the four bridge legs seated to that base initially, and prepare the exported model for later support-loss / collapse behavior in LS-PrePost or LS-DYNA workflows.\n"
        ),
        workspace_root.display(),
        config.executable.display(),
        config.stage2_product,
        stage1_run_dir.display(),
        stage1_draft_path.display(),
        stage1_out_file.display(),
        stage1_err_file
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none)".to_string()),
        draft_path.display(),
        config.k_dir.display(),
        if repair_notes.is_empty() {
            "(none yet)".to_string()
        } else {
            repair_notes.join("\n")
        },
        current_apdl,
        issue_text,
    );
    if let Some(stderr_text) = stderr_text.filter(|text| !text.trim().is_empty()) {
        prompt.push_str("\nSupplemental stderr content:\n");
        prompt.push_str(stderr_text);
        prompt.push('\n');
    }
    if prompt.len() > 220_000 {
        prompt.truncate(220_000);
        prompt.push_str("\n\n[stage2 repair prompt truncated]");
    }
    prompt
}

fn build_stage2_export_brief(
    workspace_root: &Path,
    config: &AnsysConfig,
    run_id: &str,
    run_dir: &Path,
    draft_path: &Path,
    out_file: &Path,
    err_file: Option<&Path>,
    k_file: Option<&Path>,
) -> String {
    let mut brief = format!(
        concat!(
            "OpenAnsys Stage 2 export brief\n\n",
            "Run id: {}\n",
            "Workspace root: {}\n",
            "ANSYS executable: {}\n",
            "Stage 2 product mode: {}\n",
            "Run directory: {}\n",
            "Draft APDL: {}\n",
            "ANSYS out: {}\n",
            "ANSYS err: {}\n",
            "APDL-K directory: {}\n",
            "K directory: {}\n",
            "Existing K file: {}\n\n",
            "Stage 1 status:\n",
            "- Geometry and mesh checks passed.\n",
            "- This run is the handoff point for the next-stage LS-DYNA / .K export work.\n\n",
            "Recommended next-step focus:\n",
            "1. Reuse the meshed Stage 1 model rather than restarting geometry creation.\n",
            "2. Determine which ANSYS 19.0 commands or product path on this installation can export or generate an LS-DYNA-compatible .K file.\n",
            "3. Prefer the LS-DYNA session/entry path and PR_DYNA-style product selection when capability depends on startup mode.\n",
            "4. Add explicit-dynamics-oriented setup including gravity, rigid ground/support behavior, and any needed contact / boundary setup for later support-loss collapse studies.\n",
            "5. The final .K should come from ANSYS-side export/generation, not from manually writing keyword text line by line.\n",
            "6. Preserve the current Stage 1 APDL and create Stage 2 export attempts as separate artifacts under apdl-k.\n\n",
            "Files to inspect first:\n",
            "- draft.apdl\n",
            "- ansys.out\n",
            "- .err (if present)\n",
            "- Bridge_Stage1.db or the latest .db created in the run directory\n"
        ),
        run_id,
        workspace_root.display(),
        config.executable.display(),
        config.stage2_product,
        run_dir.display(),
        draft_path.display(),
        out_file.display(),
        err_file
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none)".to_string()),
        config.apdl_k_runs_dir.display(),
        config.k_dir.display(),
        k_file
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none yet)".to_string()),
    );
    if k_file.is_none() {
        brief.push_str(
            "\nCurrent blocker:\n- No .K file was produced during Stage 1. The next iteration should focus specifically on export commands or workflow.\n",
        );
    }
    brief
}

fn build_stage2_export_request(
    workspace_root: &Path,
    config: &AnsysConfig,
    run_id: &str,
    run_dir: &Path,
    draft_path: &Path,
    out_file: &Path,
    err_file: Option<&Path>,
    stage2_dir: &Path,
) -> String {
    format!(
        concat!(
            "OpenAnsys Stage 2 export request\n\n",
            "Goal:\n",
            "- Starting from the successful Stage 1 meshed model, determine how to produce the target LS-DYNA .K file on this ANSYS 19.0 installation.\n\n",
            "Current run context:\n",
            "- Run id: {}\n",
            "- Workspace root: {}\n",
            "- Stage 2 ANSYS executable: {}\n",
            "- Stage 2 product mode: {}\n",
            "- Stage 1 run directory: {}\n",
            "- Stage 2 workspace: {}\n",
            "- Stage 1 APDL: {}\n",
            "- Stage 1 ansys.out: {}\n",
            "- Stage 1 err file: {}\n",
            "- APDL-K directory: {}\n",
            "- Target k directory: {}\n\n",
            "Stage 2 constraints:\n",
            "- Reuse the Stage 1 model and artifacts rather than regenerating geometry from scratch.\n",
            "- Keep Stage 2 attempts separate from Stage 1 artifacts and write them under apdl-k.\n",
            "- Focus on ANSYS-side export commands, product switches, resume/database usage, PR_DYNA-style product selection, or other workflow needed to produce a .K file.\n",
            "- This is an explicit dynamics setup, not a static-only export.\n",
            "- Add gravity loading and the support/contact setup needed for later collapse analysis after support loss.\n",
            "- Treat the bottom support/ground surface as rigid where appropriate for this workflow and preserve the four bridge legs in initial seated contact with it.\n",
            "- Do not manually write the final LS-DYNA keyword deck text with APDL file-writing loops.\n",
            "- The output should end at successful .K generation; downstream d3plot work is out of scope for now.\n\n",
            "Expected Stage 2 artifacts:\n",
            "- a dedicated export-oriented APDL or macro attempt\n",
            "- ANSYS output for that export attempt\n",
            "- final .K file in the k directory if successful\n"
        ),
        run_id,
        workspace_root.display(),
        config.executable.display(),
        config.stage2_product,
        run_dir.display(),
        stage2_dir.display(),
        draft_path.display(),
        out_file.display(),
        err_file
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none)".to_string()),
        config.apdl_k_runs_dir.display(),
        config.k_dir.display(),
    )
}

fn build_stage2_seed_apdl(stage1_draft_path: &Path, stage2_dir: &Path) -> String {
    format!(
        concat!(
            "! OpenAnsys Stage 2 seed APDL\n",
            "! This file is the editable/resumable APDL entrypoint for K export work.\n",
            "! You may replace or edit this file manually; Stage 2 should continue from the file kept here.\n",
            "!\n",
            "! Stage 1 reference APDL: {}\n",
            "! Stage 2 workspace: {}\n",
            "!\n",
            "! Recommended next steps:\n",
            "! 1. Resume or reuse the successful Stage 1 model/database.\n",
            "! 2. Add ANSYS 19.0 commands required for LS-DYNA / .K export or generation.\n",
            "! 3. Let ANSYS produce the .K file; do not hand-write the final keyword deck text.\n",
            "! 4. Keep export-oriented changes isolated to this Stage 2 file.\n",
            "!\n",
            "! Placeholder only for now; no automatic export logic has been generated yet.\n"
        ),
        stage1_draft_path.display(),
        stage2_dir.display(),
    )
}

struct RepairAgent {
    workspace_root: PathBuf,
    history_path: PathBuf,
    history: HistoryFile,
    history_token_limit: u64,
}

impl RepairAgent {
    fn new(workspace_root: &Path, history_path: PathBuf, history_token_limit: u64) -> Self {
        Self {
            workspace_root: workspace_root.to_path_buf(),
            history_path,
            history: HistoryFile {
                version: 3,
                session_id: format!("repair-{}", unix_seconds()),
                workspace_root: workspace_root.display().to_string(),
                last_active_at_ms: unix_millis(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_tokens: 0,
                entries: Vec::new(),
            },
            history_token_limit,
        }
    }

    fn repair(
        &mut self,
        client: &reqwest::blocking::Client,
        llm: &LlmConfig,
        prompt: String,
    ) -> Result<String> {
        self.compact_if_needed(client, llm, CompactionMode::BeforeTurn)?;
        self.history.push_user(prompt);
        self.save_history()?;

        for _ in 0..8 {
            self.compact_if_needed(client, llm, CompactionMode::MidTurn)?;
            let messages = build_messages(&self.workspace_root, &[], false, &self.history.entries);
            let reply = call_model(client, llm, messages, false)?;
            self.history
                .note_api_usage(reply.input_tokens, reply.output_tokens, reply.total_tokens);
            self.history
                .push_assistant(reply.content.clone(), reply.reasoning_content, Vec::new());
            self.save_history()?;

            match validate_apdl_candidate(&reply.content, "repair agent") {
                Ok(repaired) => return Ok(repaired),
                Err(err) => {
                    self.history.push_user(format!(
                        "Your previous reply was invalid for this repair step: {err:#}. Return only corrected ANSYS APDL text now. Do not call tools. Do not ask to inspect files. Do not explain."
                    ));
                    self.save_history()?;
                }
            }
        }

        bail!("repair agent exceeded retry limit without returning APDL")
    }

    fn compact_if_needed(
        &mut self,
        client: &reqwest::blocking::Client,
        llm: &LlmConfig,
        mode: CompactionMode,
    ) -> Result<()> {
        if !self.history.needs_compaction(self.history_token_limit) {
            return Ok(());
        }

        let resume_user = if mode == CompactionMode::MidTurn {
            self.history.last_user_content()
        } else {
            None
        };
        self.history.push_user(self.history.compaction_prompt(mode));
        let messages = build_messages(&self.workspace_root, &[], false, &self.history.entries);
        let reply = call_model(client, llm, messages, false)?;
        self.history
            .note_api_usage(reply.input_tokens, reply.output_tokens, reply.total_tokens);
        self.history.apply_compaction(reply.content, resume_user);
        self.save_history()
    }

    fn save_history(&mut self) -> Result<()> {
        self.history.last_active_at_ms = unix_millis();
        let text =
            serde_json::to_string_pretty(&self.history).context("failed to encode repair history")?;
        fs::write(&self.history_path, text)
            .with_context(|| format!("failed to write {}", self.history_path.display()))
    }
}

fn scan_input_dir(input_dir: &Path) -> Result<Vec<InputAsset>> {
    let mut assets = Vec::new();
    if !input_dir.exists() {
        return Ok(assets);
    }

    let input_root = input_dir
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", input_dir.display()))?;
    scan_dir_recursive(&input_root, &input_root, &mut assets)?;
    assets.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(assets)
}

fn scan_dir_recursive(root: &Path, dir: &Path, assets: &mut Vec<InputAsset>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            scan_dir_recursive(root, &path, assets)?;
            continue;
        }

        let Some(kind) = classify_asset(&path) else {
            continue;
        };
        let canonical = path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        let relative = canonical
            .strip_prefix(root)
            .unwrap_or(&canonical)
            .display()
            .to_string();
        let text = if kind == "text" {
            Some(
                fs::read_to_string(&canonical)
                    .with_context(|| format!("failed to read {}", canonical.display()))?,
            )
        } else {
            None
        };
        assets.push(InputAsset {
            path: canonical.display().to_string(),
            relative_path: relative,
            kind: kind.to_string(),
            text,
        });
    }
    Ok(())
}

fn classify_asset(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "md" => Some("text"),
        "png" | "jpg" | "jpeg" | "bmp" => Some("image"),
        _ => None,
    }
}

fn build_input_summary(assets: &[InputAsset]) -> String {
    let mut sections = vec!["OpenAnsys input summary".to_string()];
    sections.push(format!("Total assets: {}", assets.len()));
    let mut total_text_chars = 0_usize;
    let has_structured_text = assets.iter().any(|asset| {
        asset.kind == "text" && asset.relative_path.to_ascii_lowercase().contains("structured")
    });
    for asset in assets {
        sections.push(format!("- [{}] {}", asset.kind, asset.relative_path));
        if let Some(text) = &asset.text {
            let is_structured = asset.relative_path.to_ascii_lowercase().contains("structured");
            let remaining = MAX_SUMMARY_TEXT_CHARS_TOTAL.saturating_sub(total_text_chars);
            if remaining == 0 {
                sections.push("[text omitted: summary budget exhausted]".to_string());
                continue;
            }
            let budget = if has_structured_text && !is_structured {
                remaining.min(800)
            } else {
                remaining.min(MAX_SUMMARY_CHARS_PER_TEXT)
            };
            let excerpt = truncate_chars(text.trim(), budget);
            total_text_chars += excerpt.chars().count();
            if excerpt.chars().count() < text.trim().chars().count() {
                sections.push(format!("{excerpt}\n\n[truncated]"));
            } else {
                sections.push(excerpt);
            }
        }
    }
    sections.join("\n\n")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut excerpt = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        excerpt.push_str("...");
    }
    excerpt
}

fn contains_ansys_error(text: &str) -> bool {
    text.lines()
        .any(|line| line.to_ascii_uppercase().contains("*** ERROR ***"))
}

fn classify_ansys_run_issue(err_text: &str) -> Option<String> {
    let has_error = contains_ansys_error(err_text);
    let blocking_warnings = extract_blocking_warning_blocks(err_text);
    if !has_error && blocking_warnings.is_empty() {
        return None;
    }

    let mut report = String::new();
    if has_error {
        report.push_str("Detected ANSYS .err errors.\n\n");
    }
    if !blocking_warnings.is_empty() {
        report.push_str(
            "Detected blocking warnings that likely invalidate the model even without *** ERROR *** blocks.\n",
        );
        report.push_str("These warnings must be repaired before the run is accepted.\n\n");
        for (idx, block) in blocking_warnings.iter().enumerate() {
            report.push_str(&format!("Blocking warning {}:\n{}\n\n", idx + 1, block));
        }
    }
    report.push_str("Full .err content:\n");
    report.push_str(err_text);
    Some(report)
}

fn extract_blocking_warning_blocks(err_text: &str) -> Vec<String> {
    let lines = err_text.lines().collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut idx = 0;
    while idx < lines.len() {
        if lines[idx].to_ascii_uppercase().contains("*** WARNING ***") {
            let start = idx;
            idx += 1;
            while idx < lines.len() && !lines[idx].trim().is_empty() {
                idx += 1;
            }
            let block = lines[start..idx].join("\n");
            if is_blocking_warning_block(&block) {
                blocks.push(block);
            }
            continue;
        }
        idx += 1;
    }
    blocks
}

fn is_blocking_warning_block(block: &str) -> bool {
    let upper = block.to_ascii_uppercase();
    [
        "COMMAND IS IGNORED",
        "NOT PERMITTED",
        "NO AREAS EXIST",
        "NO VOLUMES EXIST",
        "NO ELEMENTS EXIST",
        "NO ITEMS ARE SELECTED",
        "UNDEFINED",
        "DEGENERATE",
        "CANNOT BE MESHED",
        "UNABLE TO",
    ]
    .iter()
    .any(|pattern| upper.contains(pattern))
}

fn validate_stage1_model_requirements(apdl_text: &str) -> Option<String> {
    let upper = apdl_text.to_ascii_uppercase();
    let has_et = upper.contains("\nET,") || upper.starts_with("ET,");
    let has_type = upper.contains("\nTYPE,") || upper.starts_with("TYPE,");
    let has_mesh_size = upper.contains("\nESIZE,")
        || upper.contains("\nLESIZE,")
        || upper.contains("\nAESIZE,")
        || upper.contains("\nSMRTSIZE,");
    let has_mesh_generation = upper.contains("\nVMESH,")
        || upper.contains("\nAMESH,")
        || upper.contains("\nVSWEEP,")
        || upper.contains("\nFVMESH,");

    let mut missing = Vec::new();
    if !has_et {
        missing.push("missing ET definition");
    }
    if !has_type {
        missing.push("missing TYPE assignment");
    }
    if !has_mesh_size {
        missing.push("missing mesh sizing command such as ESIZE/LESIZE/AESIZE");
    }
    if !has_mesh_generation {
        missing.push("missing actual mesh generation command such as VMESH/AMESH/VSWEEP");
    }

    if missing.is_empty() {
        return None;
    }

    Some(format!(
        concat!(
            "Stage 1 completeness validation failed even though the .err file is clean.\n",
            "This run cannot be accepted yet because the APDL did not reach a connected meshed model state.\n\n",
            "Missing requirements:\n- {}\n\n",
            "The repaired APDL must keep the geometry intent, but add the missing Stage 1 modeling steps."
        ),
        missing.join("\n- ")
    ))
}

fn validate_stage2_apdl_strategy(apdl_text: &str) -> Option<String> {
    let upper = apdl_text.to_ascii_uppercase();
    let uses_manual_file_write =
        upper.contains("*CFOPEN") || upper.contains("*CFWRITE") || upper.contains("*VWRITE");
    let looks_like_keyword_dump = upper.contains("*KEYWORD")
        || upper.contains("*NODE")
        || upper.contains("*ELEMENT_SOLID")
        || upper.contains("*MAT_")
        || upper.contains("*PART");

    if uses_manual_file_write && looks_like_keyword_dump {
        return Some(
            concat!(
                "Stage 2 strategy validation failed before export acceptance.\n",
                "The current Stage 2 APDL appears to be manually authoring the LS-DYNA keyword deck text with APDL file-writing commands.\n",
                "That is not the intended workflow for this project.\n\n",
                "Required correction:\n",
                "- Reuse/resume the successful Stage 1 model.\n",
                "- Drive an ANSYS-side .K export/generation workflow.\n",
                "- Do not write the final deck text line by line with *CFOPEN/*CFWRITE/*VWRITE loops over nodes and elements.\n"
            )
            .to_string(),
        );
    }

    None
}

fn validate_stage2_export_requirements(
    k_file: Option<&Path>,
    out_file: &Path,
    config: &AnsysConfig,
) -> Option<String> {
    let mut issues = Vec::new();

    if !ansys_out_indicates_completion(out_file) {
        issues.push(format!(
            concat!(
                "ANSYS output did not reach a clear completion marker yet.\n",
                "This usually means the launcher returned before the real Stage 2 batch work finished, ",
                "or the export path is still stuck inside a long-running write loop.\n",
                "ansys.out: {}"
            ),
            out_file.display()
        ));
    }

    match k_file {
        Some(path) => {
            if let Some(issue) = validate_k_file(path) {
                issues.push(issue);
            }
        }
        None => issues.push(format!(
            concat!(
                "No .K file was produced in the configured k directory.\n",
                "Target k directory: {}\n",
                "The repaired Stage 2 APDL must resume or reuse the Stage 1 model and add the correct export workflow so a final .K file appears there."
            ),
            config.k_dir.display()
        )),
    }

    if issues.is_empty() {
        None
    } else {
        Some(format!(
            "Stage 2 export validation failed even though the .err file did not report blocking issues.\n\n{}",
            issues.join("\n\n")
        ))
    }
}

fn wait_for_ansys_completion(out_file: &Path) -> Option<String> {
    for _ in 0..ANSYS_COMPLETION_WAIT_SECS {
        if ansys_out_indicates_completion(out_file) {
            return None;
        }
        sleep(Duration::from_millis(ANSYS_COMPLETION_POLL_MS));
    }

    Some(format!(
        concat!(
            "ANSYS execution did not reach the standard completion marker within {} seconds.\n",
            "Treat this run as incomplete even if a partial .K file already exists.\n",
            "ansys.out: {}"
        ),
        ANSYS_COMPLETION_WAIT_SECS,
        out_file.display()
    ))
}

fn ansys_out_indicates_completion(out_file: &Path) -> bool {
    let Ok(text) = fs::read_to_string(out_file) else {
        return false;
    };
    text.contains("ANSYS RUN COMPLETED")
        || text.contains("END OF INPUT ENCOUNTERED")
        || text.contains("E N D   A N S Y S   S T A T I S T I C S")
}

fn validate_k_file(path: &Path) -> Option<String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => {
            return Some(format!(
                "A .K file path was detected but metadata could not be read: {} ({err})",
                path.display()
            ));
        }
    };

    if metadata.len() == 0 {
        return Some(format!("The .K file is empty: {}", path.display()));
    }

    if metadata.len() > MAX_REASONABLE_K_FILE_BYTES {
        return Some(format!(
            concat!(
                "The .K file is unreasonably large for this bridge export and is likely a runaway partial write.\n",
                "File: {}\n",
                "Size: {} bytes\n",
                "Stage 2 must not be accepted until the export ends cleanly with a compact, complete deck."
            ),
            path.display(),
            metadata.len()
        ));
    }

    let full_text_upper = match fs::read_to_string(path) {
        Ok(text) => text.to_ascii_uppercase(),
        Err(_) => {
            let head = match read_file_head(path, K_FILE_HEAD_BYTES) {
                Ok(text) => text,
                Err(err) => {
                    return Some(format!(
                        "Failed to read the beginning of the .K file for validation: {} ({err})",
                        path.display()
                    ));
                }
            };
            let tail = match read_file_tail(path, K_FILE_TAIL_BYTES) {
                Ok(text) => text,
                Err(err) => {
                    return Some(format!(
                        "Failed to read the end of the .K file for validation: {} ({err})",
                        path.display()
                    ));
                }
            };
            format!("{}\n{}", head.to_ascii_uppercase(), tail.to_ascii_uppercase())
        }
    };

    let mut missing = Vec::new();
    for marker in ["*KEYWORD", "*NODE", "*ELEMENT", "*PART", "*SECTION", "*MAT", "*END"] {
        if !full_text_upper.contains(marker) {
            missing.push(marker);
        }
    }

    if !missing.is_empty() {
        return Some(format!(
            concat!(
                "The .K file exists but does not look structurally complete yet.\n",
                "File: {}\n",
                "Missing required markers in the checked .K text: {}\n",
                "This usually means the export is partial or wrote the wrong deck format."
            ),
            path.display(),
            missing.join(", ")
        ));
    }

    if !full_text_upper.contains("*END") {
        return Some(format!(
            "The .K file does not contain *END yet, so the export is still incomplete: {}",
            path.display()
        ));
    }

    None
}

fn read_file_head(path: &Path, max_bytes: usize) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut buffer = vec![0_u8; max_bytes];
    let bytes_read = file
        .read(&mut buffer)
        .with_context(|| format!("failed to read {}", path.display()))?;
    buffer.truncate(bytes_read);
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn read_file_tail(path: &Path, max_bytes: usize) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("failed to read metadata for {}", path.display()))?
        .len();
    let max_bytes_u64 = max_bytes as u64;
    let start = file_len.saturating_sub(max_bytes_u64);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("failed to seek {}", path.display()))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn latest_stage2_workspace(config: &AnsysConfig) -> Result<Option<PathBuf>> {
    if !config.apdl_k_runs_dir.exists() {
        return Ok(None);
    }

    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(&config.apdl_k_runs_dir)
        .with_context(|| format!("failed to read {}", config.apdl_k_runs_dir.display()))?
    {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join("current_stage2.apdl").exists() && !path.join("draft.apdl").exists() {
            continue;
        }
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        match &newest {
            Some((best_time, _)) if modified <= *best_time => {}
            _ => newest = Some((modified, path)),
        }
    }
    Ok(newest.map(|(_, path)| path))
}

fn bootstrap_existing_stage2_workspace(
    config: &AnsysConfig,
    stage2_dir: &Path,
    stage2_current_apdl_path: &Path,
    stage2_request_path: &Path,
    stage2_brief_path: &Path,
) -> Result<()> {
    if !stage2_current_apdl_path.exists() {
        let legacy_draft = stage2_dir.join("draft.apdl");
        if legacy_draft.exists() {
            let legacy_text = fs::read_to_string(&legacy_draft)
                .with_context(|| format!("failed to read {}", legacy_draft.display()))?;
            fs::write(stage2_current_apdl_path, legacy_text)
                .with_context(|| format!("failed to write {}", stage2_current_apdl_path.display()))?;
        }
    }

    if !stage2_brief_path.exists() {
        let brief = format!(
            concat!(
                "OpenAnsys Stage 2 export brief\n\n",
                "This workspace was detected from an existing apdl-k directory.\n",
                "Stage 2 workspace: {}\n",
                "Current stage2 apdl: {}\n",
                "Target k directory: {}\n"
            ),
            stage2_dir.display(),
            stage2_current_apdl_path.display(),
            config.k_dir.display(),
        );
        fs::write(stage2_brief_path, brief)
            .with_context(|| format!("failed to write {}", stage2_brief_path.display()))?;
    }

    if !stage2_request_path.exists() {
        let request = format!(
            concat!(
                "OpenAnsys Stage 2 export request\n\n",
                "Goal:\n",
                "- Run the APDL kept in current_stage2.apdl and drive it toward successful .K export.\n\n",
                "Current stage2 workspace: {}\n",
                "Current stage2 apdl: {}\n",
                "Target k directory: {}\n"
            ),
            stage2_dir.display(),
            stage2_current_apdl_path.display(),
            config.k_dir.display(),
        );
        fs::write(stage2_request_path, request)
            .with_context(|| format!("failed to write {}", stage2_request_path.display()))?;
    }

    Ok(())
}

fn newest_k_file(k_dir: &Path) -> Result<Option<PathBuf>> {
    if !k_dir.exists() {
        return Ok(None);
    }

    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(k_dir).with_context(|| format!("failed to read {}", k_dir.display()))? {
        let path = entry?.path();
        let is_k = path
            .extension()
            .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("k"))
            .unwrap_or(false);
        if !is_k {
            continue;
        }
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        match &newest {
            Some((best_time, _)) if modified <= *best_time => {}
            _ => newest = Some((modified, path)),
        }
    }
    Ok(newest.map(|(_, path)| path))
}

fn strip_code_fences(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(stripped) = trimmed.strip_prefix("```") {
        let stripped = stripped.trim_start_matches(|ch| ch != '\n');
        return stripped
            .trim_start_matches('\n')
            .trim_end_matches("```")
            .trim()
            .to_string();
    }
    trimmed.to_string()
}

fn validate_apdl_candidate(text: &str, stage_label: &str) -> Result<String> {
    let candidate = strip_code_fences(text);
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        bail!("{stage_label} returned empty APDL");
    }

    let upper = trimmed.to_ascii_uppercase();
    let looks_like_tool_transcript = trimmed.starts_with("Tool:")
        || upper.contains("TOOL: SHELL_TOOL")
        || upper.contains("ARGUMENTS: {")
        || upper.contains("\"COMMAND\":")
        || upper.contains("LET ME EXAMINE THE STAGE 1 MODEL");
    if looks_like_tool_transcript {
        bail!("{stage_label} returned tool transcript / natural-language text instead of APDL");
    }

    let has_apdl_signal = [
        "/PREP7", "/SOLU", "FINISH", "RESUME", "/FILNAME", "ET,", "MP,", "BLOCK,", "K,",
        "N,", "E,", "EDWRITE", "KEYW,", "ETCHG,", "*GET", "*DIM", "/CWD",
    ]
    .iter()
    .any(|token| upper.contains(token));
    if !has_apdl_signal {
        bail!("{stage_label} did not look like ANSYS APDL");
    }

    Ok(candidate)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        build_input_summary, classify_ansys_run_issue, scan_input_dir, strip_code_fences,
        validate_stage1_model_requirements,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn strips_markdown_fences() {
        let text = "```apdl\n/prep7\nfinish\n```";
        assert_eq!(strip_code_fences(text), "/prep7\nfinish");
    }

    #[test]
    fn builds_summary_from_assets() {
        let dir = unique_test_dir("summary");
        fs::create_dir_all(&dir).expect("create dir");
        fs::write(dir.join("a.txt"), "bridge notes").expect("write text");
        let assets = scan_input_dir(&dir).expect("scan");
        let summary = build_input_summary(&assets);
        assert!(summary.contains("OpenAnsys input summary"));
        assert!(summary.contains("bridge notes"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scans_supported_assets_only() {
        let dir = unique_test_dir("scan");
        fs::create_dir_all(&dir).expect("create dir");
        fs::write(dir.join("model.txt"), "hello").expect("write text");
        fs::write(dir.join("drawing.png"), [0_u8, 1_u8]).expect("write image");
        fs::write(dir.join("skip.doc"), "ignored").expect("write doc");

        let assets = scan_input_dir(&dir).expect("scan");
        assert_eq!(assets.len(), 2);
        assert!(assets.iter().any(|asset| asset.relative_path == "model.txt"));
        assert!(assets.iter().any(|asset| asset.relative_path == "drawing.png"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn blocking_warning_requires_repair() {
        let err = "\
*** WARNING ***\n\
Specified range of 0 to 0 is not permitted. The VEXT command is ignored.\n\
\n";
        let report = classify_ansys_run_issue(err).expect("blocking warning should require repair");
        assert!(report.contains("blocking warnings"));
        assert!(report.contains("VEXT command is ignored"));
    }

    #[test]
    fn shape_warning_alone_does_not_require_repair() {
        let err = "\
*** WARNING ***\n\
Shape testing revealed that 1 of the 28125 new or modified elements violate shape warning limits.\n\
\n";
        assert!(classify_ansys_run_issue(err).is_none());
    }

    #[test]
    fn stage1_requires_mesh_commands() {
        let apdl = "/prep7\nmp,ex,1,3e6\nsave,model,db\nfinish\n";
        let report =
            validate_stage1_model_requirements(apdl).expect("incomplete stage 1 should be rejected");
        assert!(report.contains("missing ET definition"));
        assert!(report.contains("missing actual mesh generation command"));
    }

    #[test]
    fn stage1_accepts_meshed_model_apdl() {
        let apdl = "\
/prep7\n\
et,1,solid185\n\
type,1\n\
mat,1\n\
esize,50\n\
vmesh,all\n\
finish\n";
        assert!(validate_stage1_model_requirements(apdl).is_none());
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("openansys-agent-{label}-{nonce}"))
    }
}

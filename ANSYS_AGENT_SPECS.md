# ANSYS Agent Spec Draft

## Goal

Turn this project into a dedicated ANSYS-oriented agent with minimal disruption to the existing architecture, especially:

- keep the current agent loop
- keep the current context compaction flow
- keep the same core agent structure for both the main agent and the child agent

The end product should no longer present itself as "mini-codex". It should behave and read as an ANSYS agent.

The agent should read mixed input materials, generate APDL code, run it in ANSYS 19.0, iteratively repair APDL until there are no errors, and then produce an LS-DYNA `.k` file.

## Confirmed Environment

- ANSYS executable:
  - `F:\ansys\19.0zhuchengxu\ANSYS Inc\v190\ansys\bin\winx64\ANSYS190.exe`
- ANSYS version:
  - `19.0`
- OS target:
  - Windows

## Output Layout

User-created output structure:

```text
output/
  apdl/
    <run-id-1>/
    <run-id-2>/
    ...
  k/
```

### Intended meaning

- `output/apdl/<run-id>/`
  - stores artifacts from each APDL execution attempt
  - should include at least:
    - input APDL file
    - ANSYS stdout/stderr capture
    - ANSYS-generated `.err` file when present
    - child-agent repair summary
    - revised APDL for the next attempt
- `output/k/`
  - stores final generated `.k` outputs

## End-to-End Workflow

1. Read all usable materials from an input directory.
2. Understand the task from:
   - text files (`.txt`, possibly `.md`)
   - image files containing engineering drawings of the model
3. Generate APDL code that can build the model and support later LS-DYNA export to `.k`.
4. Run the generated APDL in ANSYS 19.0.
5. Collect the run result.
6. If there are `error` messages:
   - send the current APDL
   - send the current run's error output
   - send a compressed summary if the raw error is too long
   - send recent repair history summary
   - hand the task to a child agent with independent context
7. The child agent revises the APDL.
8. Run the revised APDL again in ANSYS.
9. Repeat until there is no `error`.
10. After a successful APDL flow, generate the LS-DYNA `.k` file through the ANSYS-side workflow.

## Current Understanding of ANSYS Invocation

Mechanical APDL batch execution on Windows is generally done with the executable plus batch flags and explicit input/output files.

Canonical Windows-style form from ANSYS documentation:

```text
"<ansys-exe>" -b -i inputname -o outputname
```

For this project, a likely shape is:

```text
"F:\ansys\19.0zhuchengxu\ANSYS Inc\v190\ansys\bin\winx64\ANSYS190.exe" -b -i "<run-dir>\model.inp" -o "<run-dir>\ansys.out"
```

Additional job-name / working-directory flags may also be needed, but they are not yet confirmed for this exact installation.

## Stage 1 Success Condition

The loop stops when there is no `error` in the ANSYS run output.

Assumption from current discussion:

- if there is no `error`, LS-DYNA-side `.k` generation should succeed

This assumption should still be validated against actual ANSYS output behavior.

## Two-Stage Strategy

Chosen workflow:

1. Stage 1: generate and repair APDL until ANSYS run output contains no `error`
2. Stage 2: after Stage 1 is successful, perform `.k` export

Current discussion is focused mainly on Stage 1.

## Input Handling

### Current Requirement

There is no fixed user-provided input format.

The agent must read all relevant materials under the input area, including:

- `.txt`
- `.md` if present
- image files

There will not be `.doc` files.

### Interpretation Rule

The system should not assume only one file per section.
Multiple files may jointly describe one part of the model.

### Open Design Choice

We still need to choose whether to:

1. keep a fully loose input directory and scan everything automatically, or
2. define a light recommended structure such as:

```text
input/
  text/
  images/
```

Current user preference suggests automatic scanning is required even if a recommended structure is introduced.

## Image Understanding

Preferred direction from discussion:

- Route A: send images directly to a multimodal model

Image content is expected to be engineering drawings for the full model.

### Note

OCR alone is probably not enough for this use case because engineering drawings often contain:

- geometry relationships
- annotations
- dimension markings
- layout structure

OCR may still be useful as a helper for extracting visible text from drawings, but it should not be the only image-processing strategy.

## Child Agent Design

Chosen direction:

- Direction 2: real child agent with independent context

Requirements:

- the child agent must use the same core agent structure as the main agent
- the difference should be task focus, not a completely separate implementation
- the child agent should specialize in error-driven APDL repair

### Intended Separation

- Main agent:
  - read input materials
  - generate initial APDL
  - orchestrate runs
  - decide when to invoke repair
- Child agent:
  - only repair errors
  - receive APDL + current error context + repair history summary
  - produce revised APDL

### Child Agent Context Boundary

The child agent is not responsible for general project understanding or full workflow planning.

It is only responsible for repairing APDL based on ANSYS run errors.

## Error Handoff Contract

When invoking the child agent, pass:

1. current APDL full text
2. full ANSYS `.err` content when present
3. stderr/stdout error-relevant output from the current ANSYS run when useful as fallback/context
4. summary of recent repair attempts

User clarified that the child agent should read the full `.err` file content directly, rather than a separately extracted `errors.txt` summary file.

## Error Definition

For the current design, `error` means the error output produced when the command stream runs in ANSYS.

At this stage, the implementation does not need a more elaborate semantic classification than that.

### Primary Error Source

If ANSYS generates a `.err` file for the run, that file should be treated as the primary structured error source.

Sample observed file:

- [54278.err](D:/Desktop/openansys3/output_models/54278.err)

Observed characteristics:

- contains `*** WARNING ***` and `*** ERROR ***` blocks
- includes solver/modeling errors such as insufficient constraints
- may also include follow-on file/result errors
- may include internal termination errors such as `SIG$BREAK` or `SIG$SEGV`

### Practical Rule

Preferred order for error extraction:

1. `.err` file
2. `ansys.out`
3. stderr capture

### Current Decision

Do not introduce a required derived `errors.txt` file as the primary repair input.

Instead:

- preserve the full original `.err` file
- give the child agent the `.err` content directly
- use `ansys.out` and stderr only as supplemental context when needed

## Architecture Constraints

The implementation should avoid major rewrites.

Keep intact as much as possible:

- agent loop
- context compaction behavior
- existing session/history model

Preferred extension style:

- add ANSYS-specific orchestration around the current agent design
- avoid rebuilding the whole application around a new workflow engine

However, the user wants the final product identity to be ANSYS-specific rather than a visible generic "mini-codex" clone.
So prompts, naming, and user-facing text should be rewritten accordingly.

## Expected New Capabilities

Likely additions without changing the overall core too much:

1. input material ingestion
2. multimodal prompt assembly
3. ANSYS runner integration
4. run log parsing focused on `error`
5. child-agent repair orchestration
6. LS-DYNA `.k` export stage

## Important Unknowns

These still need to be clarified before implementation:

1. The exact ANSYS command-line invocation for APDL batch execution with `ANSYS190.exe`
2. The exact APDL / ANSYS steps required to export an LS-DYNA `.k` file in version 19.0
3. The exact file patterns to treat as images
4. Whether the final `.k` file is generated directly during APDL execution or through an additional ANSYS-side export phase
5. Whether some warning-like messages should still count as blocking failures even if they are not labeled `error`
6. The exact ANSYS 19.0 batch flags to set working directory, jobname, and product/license selection for this installation

## Near-Term Recommendation

Before code changes, confirm:

1. a reproducible command-line way to run an APDL script with `ANSYS190.exe`
2. a reproducible command-line or APDL-driven way to export the `.k` file
3. one or two real sample input folders
4. one real failing APDL example plus its ANSYS error output

Those examples will strongly reduce design guesswork.

## Sample `.k` Reference

User provided a reference LS-DYNA keyword file:

- [temp_3.5.k](C:/Users/jsk/xwechat_files/wxid_to3nx285jis412_ee1d/msg/file/2026-05/temp_3.5.k)

Observed characteristics from the file header/content:

- file size is large: about 78 MB
- header says:
  - `LS-DYNA Keyword file created by LS-PrePost(R) V4.3 (Beta)`
- it is a standard keyword-style deck beginning with:
  - `*KEYWORD`
  - `*TITLE`
- visible control/database sections include examples such as:
  - `*CONTROL_BULK_VISCOSITY`
  - `*CONTROL_ENERGY`
  - `*CONTROL_TERMINATION`
  - `*CONTROL_TIMESTEP`
  - `*DATABASE_BINARY_D3PLOT`
  - `*BOUNDARY_SPC_SET`
  - `*SET_NODE_LIST`

### Implication

The target artifact is not a tiny export stub. It is a full LS-DYNA keyword deck with model/control/database content.

This sample can later be used as a structural reference for:

- expected section patterns
- expected keyword formatting
- expected output validation checks

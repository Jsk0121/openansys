# OpenAnsys Project Notes

## Overview

OpenAnsys is a Rust-based ANSYS-oriented agent built on top of the existing mini-codex style agent loop.

The current project goal is:

1. Read bridge-related source material from `input/`
2. Generate ANSYS APDL for model creation and meshing
3. Run ANSYS in batch mode
4. Repair failed APDL through a child repair agent
5. Export an LS-DYNA `.k` file through a separate Stage 2 workflow

The project keeps the original agent architecture where possible, instead of replacing it with a separate one-off pipeline.

## Core Design

The project now uses a **two-stage ANSYS workflow**.

### Stage 1: Modeling and Meshing

Stage 1 is responsible for:

- reading `input/` assets
- building an input summary
- generating initial APDL
- creating geometry
- assigning materials / element types
- meshing the model
- saving the ANSYS database

Stage 1 success is no longer defined only by "no `.err` error".

It must also satisfy structural completeness checks such as:

- APDL contains modeling intent
- element type definition exists
- meshing is actually performed
- the model reaches a saved meshed state

### Stage 2: LS-DYNA Export

Stage 2 is responsible for:

- reusing or resuming the Stage 1 model
- converting the model into an export-ready LS-DYNA-oriented APDL flow
- running ANSYS with `-p dyna`
- producing the final `.k` file in `output/k/`

Important rule:

Stage 2 should **not** hand-write the keyword deck line by line with APDL file-writing commands.  
The intended workflow is that **ANSYS generates the `.k` file**, not the LLM.

## Current Practical State

At this point, the project can already:

- scan and summarize source files from `input/`
- generate Stage 1 APDL
- run and repair Stage 1 repeatedly
- create a Stage 2 workspace automatically
- run Stage 2 independently via `/run-stage2`
- export a `.k` file that can be executed in LS-DYNA

Current limitation:

The exported `.k` is presently closer to a **mesh/material/control deck** than a fully loaded engineering model.  
Boundary conditions and gravity/load definition still need to be enforced more explicitly in Stage 2 validation and generation.

## Repository Structure

Key directories:

- `input/`
  - source materials for the structure
- `output/`
  - generated artifacts
- `output/apdl/`
  - Stage 1 run artifacts
- `output/apdl-k/`
  - Stage 2 workspaces and export-oriented APDL
- `output/manual/`
  - manual `/run-stage2` attempt folders
- `output/k/`
  - final exported `.k` files
- `src/`
  - Rust source code

## Important Source Files

### `src/core.rs`

Main REPL / agent entrypoint.

It is responsible for:

- starting the app
- handling commands
- dispatching ANSYS workflows
- printing user-facing run summaries

Important commands:

- `/run`
  - run Stage 1, then Stage 2
- `/run-stage2`
  - skip Stage 1 and continue from the latest Stage 2 workspace

### `src/ansys.rs`

This is the main ANSYS orchestration module.

It now acts as the project’s workflow engine for:

- input bundling
- APDL generation
- ANSYS execution
- repair loops
- Stage 2 bootstrapping
- K-file validation
- resumable export runs

Important responsibilities inside `ansys.rs`:

#### 1. Configuration

`AnsysConfig` stores:

- ANSYS executable path
- input/output directories
- Stage 1 APDL directory
- Stage 2 APDL directory
- K output directory
- max repair attempts
- Stage 2 product mode, currently defaulting to `dyna`

#### 2. Input Preparation

`prepare_input_bundle()`:

- scans the `input/` directory
- classifies supported assets
- writes:
  - `output/source_context.json`
  - `output/input_summary.txt`

#### 3. Stage 1 Pipeline

`run_pipeline()`:

- prepares input
- generates initial APDL
- runs ANSYS
- repairs APDL when needed
- validates Stage 1 completeness
- creates Stage 2 workspace
- launches Stage 2 pipeline

#### 4. Stage 2 Pipeline

`run_stage2_pipeline()`:

- ensures a usable `current_stage2.apdl`
- runs ANSYS in Stage 2 mode
- checks `.err`
- checks whether ANSYS really completed
- checks whether a structurally valid `.k` exists
- invokes repair agent if export is incomplete or invalid

#### 5. Manual Stage 2 Resume

`run_stage2_only()`:

- finds the latest Stage 2 workspace
- allows the user to rerun export directly
- stores manual attempts under `output/manual/<run-id>/`

This is important because the user may already have a good APDL file and may want to continue from Stage 2 without regenerating Stage 1.

#### 6. ANSYS Process Launch

`execute_ansys()` is the shared executor for both stages.

It:

- writes the current APDL to `draft.apdl`
- launches ANSYS in batch mode
- optionally appends `-p dyna` for Stage 2
- collects `ansys.out`
- collects stderr when present
- checks `.err`
- waits for completion markers in `ansys.out`

#### 7. Repair Agent

`RepairAgent` preserves the child-agent style repair loop.

It:

- stores repair history
- supports compaction
- retries model calls
- forces the model to return only APDL

The project now rejects bad repair replies such as:

- natural-language explanations
- tool transcripts
- empty responses
- content that does not resemble APDL

#### 8. Validation

The project now includes several validation layers.

Stage 1 validation:

- model/mesh completeness checks

Stage 2 validation:

- export strategy checks
- ANSYS completion checks
- K-file structure checks

Examples of Stage 2 rejection conditions:

- ANSYS did not clearly finish
- `.k` file is missing
- `.k` file is empty
- `.k` file is unreasonably large
- `.k` file is missing required markers such as `*KEYWORD`, `*NODE`, `*PART`, `*MAT`, `*END`
- APDL is trying to manually print a keyword deck with `*CFOPEN/*CFWRITE/*VWRITE`

## Stage 2 Workspace Convention

Each Stage 2 workspace under `output/apdl-k/` is expected to contain:

- `current_stage2.apdl`
  - the APDL that should be run for Stage 2
- `stage2_export_brief.txt`
  - compact summary of Stage 1 and Stage 2 intent
- `stage2_export_request.txt`
  - prompt-oriented request for export repair/generation

If `current_stage2.apdl` is still only a placeholder, `/run-stage2` will refuse to run and will ask for a real APDL file to be placed there first.

## User Workflow

Typical workflow:

1. Put bridge source files into `input/`
2. Run:

```powershell
cargo run -- --auto
```

3. In the REPL, run:

```text
/run
```

This will:

- build Stage 1
- attempt repairs if needed
- create Stage 2 workspace
- attempt K export

If Stage 1 is already satisfactory and only Stage 2 needs work:

```text
/run-stage2
```

## Why the Two-Stage Split Matters

The split between `apdl` and `apdl-k` is intentional.

It gives the project:

- resumability
- cleaner debugging
- lower context pressure
- manual intervention points
- a stable handoff between geometry/mesh work and export work

This matches the user’s requirement for progressive disclosure: each stage should reveal only the next necessary working surface, instead of forcing the entire problem into one huge generation step.

## Recommended Next Improvements

The most valuable next steps are:

1. enforce explicit boundary-condition checks in Stage 2
2. enforce gravity/load checks in Stage 2
3. distinguish "mesh-only K file" from "analysis-ready K file"
4. improve Stage 2 prompt guidance for LS-DYNA export commands
5. optionally add stronger reporting around what loads/constraints were found

## Summary

The project has already moved from a simple APDL generation loop into a resumable ANSYS workflow system with:

- structured input preparation
- Stage 1 modeling/meshing
- Stage 2 LS-DYNA export
- child-agent repair
- validation beyond `.err`
- manual continuation support

The framework is now strong enough that the next work should focus less on orchestration and more on engineering correctness of the exported model.

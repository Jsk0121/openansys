# OpenAnsys Agent

OpenAnsys Agent is a Rust-based local CLI workflow for ANSYS-oriented automation.

It reads engineering source materials from `input/`, generates ANSYS APDL, runs ANSYS in batch mode, repairs failed APDL attempts, and prepares LS-DYNA `.k` export through a two-stage workflow.

> [!WARNING]
> This project is intended for learning and local engineering workflow exploration. Generated APDL and LS-DYNA keyword files must be reviewed by a qualified engineer before real analysis, design decisions, or production use.

## Features

- Rust command-line agent
- ANSYS APDL generation from local input materials
- Stage 1 modeling and meshing workflow
- Stage 2 LS-DYNA `.k` export workflow
- ANSYS `.err` and output-driven repair loop
- Resumable interactive sessions
- Local skill discovery through `SKILL.md`
- GitHub Pages project site in `docs/`

## Repository Layout

```text
.
├── .github/workflows/      # GitHub Pages deployment workflow
├── docs/                   # Static GitHub Pages site
├── input/                  # Local source materials for model generation
├── output/                 # Local generated run artifacts, ignored by git
├── src/                    # Rust source code
├── Cargo.toml
├── Cargo.lock
└── README.md
```

## Configuration

Configuration is loaded from `.env` first, then from normal environment variables.

Common variables:

```env
OPENANSYS_API_KEY=your_api_key_here
OPENANSYS_BASE_URL=https://api.openai.com/v1
OPENANSYS_ANSYS_EXE=F:\ansys\19.0zhuchengxu\ANSYS Inc\v190\ansys\bin\winx64\ANSYS190.exe
```

Compatible fallback names are also supported:

- `MINI_CODEX_API_KEY` or `OPENAI_API_KEY`
- `MINI_CODEX_BASE_URL` or `OPENAI_BASE_URL`
- `ANSYS_EXE_PATH`

## Install

Install from the repository root:

```bash
cargo install --path .
```

Then run:

```bash
openansys-agent
```

For development, run directly with Cargo:

```bash
cargo run -- --auto
```

## Usage

Put model source materials under `input/`, then start the CLI:

```bash
cargo run -- --auto
```

Inside the interactive session:

```text
/run
```

This starts the Stage 1 APDL generation and repair workflow, then prepares and runs Stage 2 export.

To resume only the latest Stage 2 export workspace:

```text
/run-stage2
```

Useful commands:

- `/help` - show interactive help
- `/continue` - retry the previous assistant turn
- `/auto on` - enable automatic shell command approval
- `/auto off` - require approval before shell commands
- `/exit` - exit the session

## GitHub Pages

This repository includes a static website in `docs/` and a GitHub Actions workflow for Pages deployment.

To publish:

1. Upload the repository contents to GitHub. Do not upload the zip file itself; upload the extracted files.
2. Confirm the repository root contains `docs/index.html`.
3. Open **Settings -> Pages**.
4. Set **Build and deployment** to **GitHub Actions**.
5. Open **Actions** and wait for `Deploy GitHub Pages` to finish successfully.

The site URL will look like:

```text
https://<github-user>.github.io/<repository-name>/
```

## GitHub Upload Checklist

The GitHub repository root should contain:

```text
.github/
docs/
src/
Cargo.toml
Cargo.lock
README.md
```

The `docs/` directory should contain:

```text
docs/index.html
docs/styles.css
docs/script.js
docs/.nojekyll
```

If GitHub Actions reports `tar: docs: Cannot open: No such file or directory`, the `docs/` folder was not uploaded correctly.

## Safety Notes

- `.env` is ignored and should not be uploaded.
- `output/` is ignored because it contains generated local run artifacts.
- `target/` is ignored because it contains Rust build outputs.
- ANSYS and LS-DYNA outputs should be checked manually before engineering use.

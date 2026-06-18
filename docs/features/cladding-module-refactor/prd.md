# PRD: Cladding Module Refactor

## Motivation
The Rust implementation in `cladding/src` has several modules that now carry too many unrelated responsibilities:

- `cladding/src/cli.rs` is about 2,000 lines and contains argument parsing, command dispatch, project-root resolution, `init`/`check`/`up`/`down`, run/logs helpers, expose/inject supervision, and many command-specific tests.
- `cladding/src/runtime.rs` is about 1,200 lines and mixes runtime data types, component pod construction, mount construction, socket path helpers, sidecar command construction, and runtime path collection.
- `cladding/src/podman.rs` is about 1,100 lines and mixes Podman availability checks, image build, command tracing/quoting, direct runtime execution, Podman discovery, command builders, filesystem preparation, and tests.
- `cladding/src/config.rs` is about 900 lines and mixes public config data types, JSON parsing, config writing, mount parsing, component validation, name normalization, and tests.

The files are still cohesive at the product level, but they are becoming harder to review because small behavior changes require navigating broad files and broad test modules. This proposal splits the large files into submodules while preserving the current public module names and behavior.

## Problem statement
The current module shape makes unrelated changes appear coupled:

- A mount parsing change in `config.rs` is physically close to name derivation and default config rendering.
- A direct runtime mount change in `runtime.rs` is physically close to proxy bridge sidecar command generation and component construction.
- A Podman command builder change in `podman.rs` is physically close to runtime cleanup and running-project discovery.
- A CLI parsing change in `cli.rs` is physically close to long-running inject supervision and command execution helpers.

The immediate need is not to redesign the APIs. It is to make the existing code easier to navigate, test, and review without changing command behavior, config schema, runtime names, or Podman command semantics.

## Proposal
Refactor the four large modules into Rust submodules that preserve existing external paths:

- Keep `crate::config::{ExecutionConfig, load_cladding_config_v2, ...}` available.
- Keep `crate::runtime::{RuntimeSpec, RuntimePod, RuntimeMount, ...}` available.
- Keep `crate::podman::{runtime_create, list_running_projects, ...}` available.
- Keep `cladding::cli::run()` and `cladding::cli::print_error_and_exit()` behavior unchanged.

The preferred pattern is Rust's paired parent-file layout: keep the parent module file and add sibling submodule files under a same-named directory. For example, keep `cladding/src/runtime.rs` as the parent module that declares private submodules, and add files such as `cladding/src/runtime/types.rs`, `cladding/src/runtime/components.rs`, `cladding/src/runtime/mounts.rs`, and `cladding/src/runtime/sockets.rs`.

This is a source organization refactor only. The compiled behavior should remain equivalent.

### Config split
Keep `cladding/src/config.rs` as the parent module and split internals into `cladding/src/config/*.rs` around the existing responsibilities:

- `types.rs`: `DEFAULT_COMPONENT_IMAGE`, `ExecutionComponentConfig`, `ExecutionConfig`, `MountTarget`, and `ResolvedMountConfig`, including simple impl methods.
- `load.rs`: `load_cladding_config_v2`, top-level key validation, component parsing, and config scalar helpers.
- `mounts.rs`: `parse_mounts_v2`, `parse_mount_targets`, `validate_mount_keys`, and mount-path validation.
- `init.rs`: `write_default_cladding_config`, name derivation, and name normalization.

`config.rs` should re-export the public types and functions needed today. Parser helpers should remain private to the config module unless a test or caller already needs them.

### Runtime split
Keep `cladding/src/runtime.rs` as the parent module and split internals into `cladding/src/runtime/*.rs` by runtime model responsibility:

- `types.rs`: `RuntimeSpec`, `RuntimePod`, `RuntimePlacement`, `RuntimeContainer`, `RuntimeEnvVar`, `RuntimeMount`, `RuntimeCustomMount`, `RuntimeMountSource`, and `RuntimeNames`.
- `components.rs`: `RuntimeSpec::build`, `build_proxy_pod`, `build_agent_pod`, `build_nw_sandbox_pod`, `build_fs_sandbox_pod`, labels, container naming, and sidecar container construction.
- `mounts.rs`: built-in mount lists, custom mount application, custom mount conversion, mount source conversion, and required host path collection.
- `sockets.rs`: socket directory constants, runtime socket path helpers, scoped socket mounts, generated socket directory collection, and generated-runtime path classification.
- `commands.rs`: shell supervisor command builders if separating them keeps `components.rs` focused.

The exact file names may vary if the implementation finds a cleaner boundary, but mounts and socket helpers should not remain buried in the same file as all component builders.

### Podman split
Keep `cladding/src/podman.rs` as the parent module and split internals into `cladding/src/podman/*.rs` around Podman operations:

- `command.rs`: Podman runtime option handling, command construction helpers, tracing, formatting, shell quoting, and success checking.
- `runtime.rs`: `runtime_create`, `runtime_cleanup`, `pod_create`, `pod_rm`, `container_run`, `container_rm`, runtime pod iteration, socket directory preparation, and empty mask preparation.
- `build.rs`: `podman_build_image`.
- `discovery.rs`: `podman_required`, `runsc_available`, `list_running_projects`, `podman_container_exists`, JSON label parsing, running pod inspection, and runtime availability helpers.
- `mounts.rs`: Podman `--volume` argument construction, empty-dir volume names, generated empty mask directory collection, and volume-name sanitization.

The split should avoid circular dependencies by keeping low-level command helpers independent of runtime-specific builders.

### CLI split
Keep `cladding/src/cli.rs` as the parent module and split internals into `cladding/src/cli/*.rs` along command families:

- `args.rs`: `Cli`, `CommandSpec`, `ExposeArgs`, `InjectArgs`, `RunWithScissorsTarget`, `LogsTarget`, and CLI value parsers.
- `context.rs`: `Context`, `find_project_root`, `resolve_project_root`, and current project root helpers.
- `commands.rs`: command dispatch and small command functions for `build`, `init`, `check`, `up`, `down`, `destroy`, and `ps`, or separate files for `lifecycle.rs` and `check.rs` if that keeps ownership clearer.
- `exec.rs`: `cmd_run`, `cmd_run_with_scissors`, `cmd_logs`, `cmd_reload_proxy`, `run_podman_exec`, workdir resolution, runtime naming helpers, and env handling.
- `expose.rs`: `cmd_expose`, blocking expose command construction, and expose-specific helpers.
- `inject.rs`: `cmd_inject`, socket path helpers, socat checks, child supervision, cleanup, and inject command builders.
- `errors.rs`: `print_error_and_exit` if keeping it out of the command dispatch module improves clarity.

The CLI submodules may keep command functions private and expose only `run()` plus `print_error_and_exit()` from `cli.rs`.

## Suggested implementation shape
Implement the refactor in dependency order:

1. Split `config` first because it has the smallest external dependency surface.
2. Split `runtime` next because `podman` and `cli` import its public types.
3. Split `podman` after runtime so command builders can import the new runtime module paths.
4. Split `cli` last because it imports all other modules and has the broadest internal test coverage.
5. Run `cargo fmt` and `cargo test -p cladding` after each major split when practical.

Each step should be mostly mechanical moves plus visibility adjustments. Prefer `pub(super)` and `pub(crate)` over broad `pub` exports for helpers that are only shared inside the parent module.

When moving tests, place each test module next to the key logic it exercises. For example, mount parsing and mount validation tests should live in the config/runtime/podman mount submodule that owns that logic, while command-builder tests should live in the command-builder submodule. Do not leave all tests in the parent module merely because that was their original location.

## Non-goals
1. Do not change `cladding.json` schema, validation messages, or default generated config.
2. Do not change runtime pod/container names, mounted paths, generated socket paths, or proxy/inject behavior.
3. Do not change Podman command semantics, flags, cleanup order, or discovery labels.
4. Do not redesign the CLI command structure or user-facing help text beyond what is necessary for moved types to compile.
5. Do not rewrite tests into a new style unless moving them next to the extracted code is required to keep private helper coverage.

## Verification
Verification should include:

- `cargo fmt --check`
- `cargo test -p cladding`
- A source-level check that no `mod.rs` files were introduced for these modules; parent files should be `config.rs`, `runtime.rs`, `podman.rs`, and `cli.rs`.
- A public API check by compiling current callers without changing imports from `crate::config`, `crate::runtime`, or `crate::podman`.

If a moved test needs access to a helper, place that test in the submodule that owns the helper rather than making the helper public just for tests. Integration-style tests may stay one layer up only when they intentionally exercise orchestration across multiple submodules.

## Success criteria
- `config.rs`, `runtime.rs`, `podman.rs`, and `cli.rs` are focused parent modules with implementation split into same-named submodule directories.
- Tests are colocated with the submodule that owns the behavior under test.
- Existing public imports used by `cladding/src/main.rs`, `cladding/src/cli`, and cross-module callers still compile.
- `cargo test -p cladding` passes.
- The refactor is accumulated as uncommitted changes with no behavior changes intentionally included.

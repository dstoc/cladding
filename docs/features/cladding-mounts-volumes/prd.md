# PRD: Cladding Mounts + Volumes

## Objective
Enable users to configure additional container mounts in `cladding.json` via a single `mounts` list, supporting:
- Named Podman volumes mounted into both `cli-app` and `sandbox-app`.
- Bind mounts with explicit `mountPath`, optional `hostPath`, and `readOnly` semantics.
- EmptyDir mounts when neither `hostPath` nor `volume` is provided.

This should extend the current fixed mount set without breaking existing behavior.

## Use Cases
1. Persist tool caches across runs (e.g., npm cache) in a shared named volume.
2. Mount host directories (or empty dirs) at specific locations for build or runtime needs.
3. Override the default workspace mount to point at a different host path or to be read-only.

## Functional Requirements
1. **Config schema**
   - Extend `cladding.json` with optional key:
     - `mounts`: array of objects with fields:
       - `mount` (required): absolute container mount path.
       - `hostPath` (optional): if present, bind from host.
       - `volume` (optional): if present, use named Podman volume.
       - `readOnly` (optional, default `false`).
       - `hostPath` and `volume` are mutually exclusive; error if both are set.
2. **Containers targeted**
   - Apply `volumes` and `mounts` only to `cli-app` and `sandbox-app`.
   - Both pods receive identical mounts.
3. **Named volumes**
   - A mount with `volume` creates/uses named Podman volume: `<cladding_name>-<volume>`.
   - The volume is mounted at the container path specified in `mount`.
   - Volumes are read-write by default; ignore `readOnly` (or treat as error if set).
4. **Bind mounts**
   - `mount` is required and must be an absolute path; error if not.
   - `mount` values must be unique; error if repeated.
   - `readOnly` defaults to `false`.
   - If `hostPath` is present:
     - If absolute, use as-is.
     - If relative, resolve relative to the `.cladding` directory.
   - If neither `hostPath` nor `volume` is present:
     - Use `emptyDir` for that mount.
     - Mount it as read-only.
     - Each pod gets its own `emptyDir` (not shared across pods).
5. **Default mounts + override order**
   - Apply mounts in this order: built-in defaults first, then `cladding.json` mounts.
   - If a custom mount’s `mount` path matches a default mount path, the custom mount overrides the default for that path.
   - The `.cladding` mask at `/home/user/workspace/.cladding` remains in place even if the workspace mount is overridden.
6. **Error handling**
   - Validation failures should report the specific invalid field and file path.
   - Duplicate mount paths should be a hard error.

## Non-Goals
- Changing which pods receive mounts beyond `cli-app` and `sandbox-app`.
- Adding read-only options to named volumes.
- Introducing mount propagation, subPath, or SELinux options.
- Altering the existing default mount set (other than allowing overrides by path).

## Design Considerations
- Keep the config format terse and JSON-friendly for hand edits.
- Provide clear error messages, especially for invalid paths or duplicates.
- Default behavior should preserve current mounts when the new config keys are absent.
- Ensure the ordering and override behavior is explicit and deterministic.

## Technical Considerations
- `cladding/src/config.rs` currently parses a limited schema; it must be extended to read and validate `mounts` with `hostPath`/`volume`/`emptyDir`.
- `cladding/src/assets.rs::render_pods_yaml` currently uses string replacement only. The new mount logic likely needs structured YAML handling:
  - Parse the embedded `pods.yaml` into a data model, apply mount/volume mutations, and serialize back to YAML.
  - Alternatively, replace the embedded template with programmatic YAML generation for the relevant sections.
- Podman named volume support in `podman play kube` should be used. The intended YAML structure is:
  - `volumes` entries referencing a named claim (e.g., `persistentVolumeClaim` with `claimName`).
  - Ensure `cladding up` creates missing volumes (`podman volume create <name>`) before `podman play kube`.
- The existing masked `.cladding` mount is a separate `emptyDir`; it must remain even if the workspace mount path is overridden.

## Success Metrics
- Users can add named volumes via `cladding.json` and observe data persistence across `cladding down`/`up` cycles.
- Users can add or override bind mounts with `mounts` and see the expected host path or emptyDir at runtime.
- Attempts to use duplicate or non-absolute mount paths fail fast with clear errors.
- Existing users with no new config keys see no behavioral change.

## Open Questions
- Confirm the exact YAML schema Podman expects for named volumes (`persistentVolumeClaim` vs. Podman-specific extension) and whether `podman play kube` auto-creates volumes or requires explicit `podman volume create`.
- Determine whether `pods.yaml` should remain an embedded template or be replaced with programmatic YAML generation for long-term maintainability.

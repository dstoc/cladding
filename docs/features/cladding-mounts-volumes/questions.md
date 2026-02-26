# Questions: Cladding mounts + volumes

1. Q: Which containers should apply the new `volumes` and `mounts` from `cladding.json` (e.g., `sandbox-app` only, `cli-app` only, both, or also proxy/init containers)?
   A: Only `cli-app` and `sandbox-app`, with identical treatment.
2. Q: Should `volumes` create named Podman volumes (K8s `persistentVolumeClaim`-like), or should they be `hostPath` mounts? If named volumes, what naming scheme do you want (e.g., `<cladding_name>_<volume_name>`)?
   A: Podman volumes, named `<cladding_name>-<volume_name>`.
3. Q: For the new `mounts` entries, should `mount` (mountPath) be required and unique per list, and should we error if two mounts target the same `mount` path?
   A: `mount` is required, and it is an error to repeat the same mountPath.
4. Q: When a custom `mounts` entry targets an existing default mount (like `/home/user/workspace` or `/opt/tools`), should it replace just that mount or also remove the paired “mask” mount at `/home/user/workspace/.cladding`?
   A: Replace just that mount; keep the mask mount.
5. Q: Should `volumes` be mounted into containers automatically, or do they only define named volumes for later use (e.g., via `mounts` referencing them)? If auto-mounted, where should they mount in the container?
   A: Auto-mount. The map value is the container mount path (e.g., `"npm": "/home/user/.npm"` mounts there).
6. Q: For `mounts` with `hostPath` specified, should paths be resolved relative to the directory that contains `.cladding` (project root) as you suggested, or relative to `.cladding` itself? Also, should we allow absolute host paths?
   A: Resolve relative to `.cladding`. Allow absolute host paths.
7. Q: For `mounts` with no `hostPath`, do you want each pod to use its own `emptyDir`, or should we use a shared named volume? Also, should the mount be read-only?
   A: Use per-pod `emptyDir` and mount it read-only.
8. Q: For `volumes`, are they read-write by default, and should there be any way to mark them read-only?
   A: Read-write by default; no read-only option.
9. Q: Should we require `mount` (mountPath) to be an absolute path, and error if it’s not?
   A: Yes, require absolute.
10. Q: You asked for `./docs/FEATURE/prd.md` in step 4, but step 1 says to create the feature folder under `./docs/features/FEATURE`. Which location should I use for the PRD file?
    A: Use `docs/features/FEATURE/...`.

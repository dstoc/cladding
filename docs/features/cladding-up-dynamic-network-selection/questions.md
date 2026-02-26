# Questions: Dynamic network selection in `cladding up`

1. Q: Should `cladding-N` networks be a single global pool shared across all projects, or should each project still have a stable/project-specific network identity (just no longer persisted in `cladding.json`)?
   A: Shared across all projects.
2. Q: For selecting an “unused” `cladding-N` network, should we treat a network as reusable only when it has no attached containers at all (including non-cladding containers), or only when it has no running cladding pods attached?
   A: Reusable when it has no running cladding pods attached; assume no non-cladding usage.
3. Q: How should we handle existing `.cladding/cladding.json` files that still contain `subnet` after this change: fail with a clear migration error, or ignore `subnet` as deprecated and continue?
   A: No special treatment; remove `subnet` from schema.
4. Q: Should `cladding init` stop creating any Podman network entirely, with all network allocation/creation deferred to `cladding up`?
   A: Yes. `cladding init` should not do anything with networks; all network work moves to `cladding up`.
5. Q: When a project is brought down and later up again, should it reuse its previous `cladding-N` network if available, or is it acceptable for it to attach to any currently-unused `cladding-N` network each time?
   A: No need to reuse; use any unused network.
6. Q: Should `cladding-N` keep using the existing `10.90.N.0/24` mapping with `N` in `0..255` (max 256 pool networks), or do you want a larger/different range?
   A: Keep `10.90.N.0/24` with `N` in `0..255`.
7. Q: Should `cladding ps` user-facing output be updated to show each running project’s assigned `cladding-N` network, or should any `ps` changes be internal-only (no output change)?
   A: Internal only; no output change required.

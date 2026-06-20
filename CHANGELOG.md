# Changelog

## [0.3.0](https://github.com/dstoc/cladding/compare/v0.2.2...v0.3.0) (2026-06-19)


### ⚠ BREAKING CHANGES

* replace expose with a blocking version that uses socat
* switch to UDS-based network isolation

### Features

* add `cladding inject` ([9556019](https://github.com/dstoc/cladding/commit/9556019c311e7a9c8f57e700d9819fa2f9468b84))
* add option to use the gvisor runtime ([26ab196](https://github.com/dstoc/cladding/commit/26ab19689b21cf547f8ab01fb6a2cae4b61d457a))
* add verbose mode to cladding up/down ([edee308](https://github.com/dstoc/cladding/commit/edee3087f527120a7fd3c7e06f280077fd224783))
* Implement inline UDS bridges for cladding runtime ([f08f5fb](https://github.com/dstoc/cladding/commit/f08f5fb5d4bf27dabfd9b3b97a9a261754952044))
* Move generated scripts into runtime state ([90596bd](https://github.com/dstoc/cladding/commit/90596bd1a0856db5c96ef2638f5b10fbe12c07c0))
* replace expose with a blocking version that uses socat ([2d2171d](https://github.com/dstoc/cladding/commit/2d2171dd57f702c7bbca6c0520139084c036a2b8))
* Restrict fs-sandbox default filesystem access ([e2434a6](https://github.com/dstoc/cladding/commit/e2434a6bd30ec4bfc1725a94c0393bb5e12abc32))
* switch from kube play to direct podman runtime ([d88f2bc](https://github.com/dstoc/cladding/commit/d88f2bc9559a04d1c9f9752315b391d22fb6b31d))
* switch to UDS-based network isolation ([f83bd75](https://github.com/dstoc/cladding/commit/f83bd75fd4aa99da7b350c931279aea4390f824e))

## [0.2.2](https://github.com/dstoc/cladding/compare/v0.2.1...v0.2.2) (2026-06-15)


### Miscellaneous Chores

* force release 0.2.2 ([6183ab0](https://github.com/dstoc/cladding/commit/6183ab019349f075c3c8e5824fb21fb06c0287d3))

## [0.2.1](https://github.com/dstoc/cladding/compare/v0.2.0...v0.2.1) (2026-06-15)


### Miscellaneous Chores

* force release 0.2.1 ([dde47ea](https://github.com/dstoc/cladding/commit/dde47eade5049ab008ccd266c7a6bffce157cf69))

## [0.2.0](https://github.com/dstoc/cladding/compare/v0.1.2...v0.2.0) (2026-06-15)


### ⚠ BREAKING CHANGES

* normalize config layout
* simplify container/pod/config naming

### Features

* add `cladding logs` ([0c27bfd](https://github.com/dstoc/cladding/commit/0c27bfd351285ba871a1354d2046fe41acc8e3f9))
* add optional fs sandbox ([69d17d0](https://github.com/dstoc/cladding/commit/69d17d0b403eb750602822457ba16d4e9836d6d9))
* normalize config layout ([b5cbdd5](https://github.com/dstoc/cladding/commit/b5cbdd56b08022ac77d8e9eb748623adc52aeae1))
* simplify container naming e.g. &lt;name&gt;-agent-agent to &lt;name&gt;-agent-instance ([909db90](https://github.com/dstoc/cladding/commit/909db90bc91df827e9e0c92c1665b32c7a34305e))
* simplify container/pod/config naming ([8f53953](https://github.com/dstoc/cladding/commit/8f53953b78a18d09e1f67cc1311c9a862ff9e75f))

## [0.1.2](https://github.com/dstoc/cladding/compare/v0.1.1...v0.1.2) (2026-06-06)


### Miscellaneous Chores

* force 0.1.2 release for working binary pipeline ([95284f7](https://github.com/dstoc/cladding/commit/95284f77e99e278321c1f8a7fe3d835d9da90419))

## [0.1.1](https://github.com/dstoc/cladding/compare/v0.1.0...v0.1.1) (2026-06-06)


### Features

* `up` and `run` check project status, error on conflict ([ecc4737](https://github.com/dstoc/cladding/commit/ecc47373f3180c49ab3f435e7bae4a14f8d1ef5c))
* add `cladding ps` ([6a9c13c](https://github.com/dstoc/cladding/commit/6a9c13ca895e315dc609b912278280ad23f61899))
* add cladding expose &lt;cli-port&gt; [host-port] ([a661cb5](https://github.com/dstoc/cladding/commit/a661cb5a392fef05aee3c4731a368e7bedb474a8))
* add run-with-scissors ([7ca34f6](https://github.com/dstoc/cladding/commit/7ca34f6bb278da9ef71b92c3ccf73a3d251c6435))
* add sandbox-only mounts ([91c94c6](https://github.com/dstoc/cladding/commit/91c94c65c644028da03222d323fda13b323178dd))
* add user configurable volumes/mounts ([bbca499](https://github.com/dstoc/cladding/commit/bbca499dba705b8475ea91e08781e4d0a7fb6764))
* allocate network dynamically during `up` ([433418b](https://github.com/dstoc/cladding/commit/433418b4cfa1260bb77958327800dd0defcfb2e0))
* allow ignore of default mounts ([730652f](https://github.com/dstoc/cladding/commit/730652fd6eec5c7ee45577adf1da5a6e4d85da4b))
* check identifies missing scripts ([b869a92](https://github.com/dstoc/cladding/commit/b869a927e85e9ceb6e5750414c27f1e99e6b6de3))
* cladding run hooks sigint/term to pass to container process ([fa54888](https://github.com/dstoc/cladding/commit/fa54888660f79b4ac91edf9ec7a6c055dd9be21e))
* detect script changes and add `..init --update-scripts` ([e4d6a5d](https://github.com/dstoc/cladding/commit/e4d6a5d82d150c4e15e58b49aecaf0fa74d534e3))
* init creates home, tools ([f031969](https://github.com/dstoc/cladding/commit/f0319695484e99000d2de278c0f7e0a3cf6901b6))
* port cladding to rust ([5c4f5d5](https://github.com/dstoc/cladding/commit/5c4f5d57de099f2d58a3dab0eee030baaf058abc))
* **run:** pass through --env X --env Y=Z ([ae3d76f](https://github.com/dstoc/cladding/commit/ae3d76f11bcfd0cd893b4fc5331ccb527fe591f8))


### Bug Fixes

* always pass -t in run ([74dc557](https://github.com/dstoc/cladding/commit/74dc55706d0c60c1e37baf2847927b15b4f32bcc))
* cladding build always writes mcp-run/run-with-network ([a90cce8](https://github.com/dstoc/cladding/commit/a90cce83801aeb6e14aadc325fdf42b6bf96a959))
* cladding run cwd resolution when /home/user/workspace is overridden ([3054a09](https://github.com/dstoc/cladding/commit/3054a0981bfe1fa51ba5f49d32671aea18be96be))
* materialize directories recursively ([276b3f8](https://github.com/dstoc/cladding/commit/276b3f8a1032516787e5f70de43ba32e547cacb2))
* only use pkill for non-interactive runs ([4176978](https://github.com/dstoc/cladding/commit/4176978bbefd6d00716070c5cc76a8fd29aabab7))
* remove explicit path checks in favor of hostPath ([86fcc99](https://github.com/dstoc/cladding/commit/86fcc99da534a649c0749eed0ad924e6cae62d98))
* revert to no tty for non-interactive run ([4d8093e](https://github.com/dstoc/cladding/commit/4d8093e6120474f850b7400605c5bea88e73ca26))
* use empty config map instead of emptyDir for masking due to forced "tmpcopyup" ([37725f0](https://github.com/dstoc/cladding/commit/37725f0ccc7d362b74aef624a002236bcef9927c))
* use podman if we need to cross-compile mcp-run for linux ([f85db77](https://github.com/dstoc/cladding/commit/f85db77e8e0245d2c75a01a8a6ddb60066162e87))

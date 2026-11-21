# arkpkg

The `arkpkg` package manager and `arkpkg-build` packager for
[arkanaOS](https://github.com/arkana-team/arkanaOS).

> [!WARNING]
> This project is under active development and is largely AI-generated.
> The `main` branch does not guarantee stability, compatibility, or correct
> behaviour; pin a commit before relying on it.

## Building

The supported toolchain is Rust **1.99** (any newer stable release works).
Everything installs through `cargo`:

```sh
cargo build --release
cargo test --all-targets          # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Two binaries are produced: `arkpkg` (install/remove/verify/search) and
`arkpkg-build` (pack a package directory into an `.ark` archive).

### Container build

The checked-in `Dockerfile` / `docker-compose.yml` provide the same environment CI
uses. It is the recommended way to run tests against a fake root:

```sh
docker compose build
docker compose up -d
docker exec arkpkg-dev cargo test --all-targets
```

`ARKPKG_ROOT` points at `/workspace/fake_root` inside the container, so tests
never touch the host filesystem.

## Usage

```sh
# Install a local package archive or a package name from a configured repository
arkpkg install ./hello-1.0.0-x86_64.ark
arkpkg install hello

# Only resolve against a single repository
arkpkg install hello --repo main

# Inspect, list, verify, remove
arkpkg list
arkpkg info hello
arkpkg verify hello
arkpkg remove hello

# Search installed packages and configured repositories
arkpkg search hello

# Repositories (HTTPS only)
arkpkg repo add main https://arkana-team.github.io/arkpkg-repo
arkpkg repo list
arkpkg repo remove main
```

Global flags: `-y/--yes`, `-n/--no`, `-f/--force`, and `--root <DIR>` (also read
from `ARKPKG_ROOT`). `--root` installs into an alternative root, which is how the
test-suite exercises the installer without privileged access.

Exit codes are stable and scriptable — see `src/errors.rs`: `2` package not found,
`3` missing dependency, `4` architecture mismatch, `5` verification failure,
`6` already installed, `7` conflict, `8` invalid package, `9` permission denied,
`10` user cancelled, `1` generic.

## Building a package

A package source directory contains exactly two entries:

```text
hello/
├── ARKPKG              # metadata (see below)
└── package/            # payload, mirrored into the target root
    └── usr/bin/hello
```

Pack it with `arkpkg-build`:

```sh
arkpkg-build examples/hello -o dist/
# writes dist/hello-1.0.0-x86_64.ark (tar.lz4)
```

`examples/hello` and `examples/bash` are complete, buildable examples.

### `ARKPKG` metadata

Line-oriented `Key: value` format with `#` comments. `Name`, `Description`,
`Version` and `Arch` are required; the rest are optional. Multi-value fields
(`Dependencies`, `Provides`, `Conflicts`) accept one item per line, comma
separated values, or an inline value after the colon.

```text
Name: bash
Description: GNU Bourne Again Shell
Version: 5.3.0
Arch: x86_64                   # or "any" / "all"
URL: https://www.gnu.org/software/bash/
License: GPL-3.0
Maintainer: arkanaOS Team
Dependencies:
glibc>=2.41
ncurses>=6.5
Provides:
bash=/usr/bin/bash
sh=/usr/bin/bash
Conflicts:
dash
busybox-sh
```

* Versions support an optional epoch (`1:2.3.4`) and `~` pre-release ordering,
  following Debian-ish rules (`2.0-alpha < 2.0`).
* `Dependencies` accepts `=`, `==`, `!=`, `<`, `<=`, `>`, `>=`.
* `Provides` entries are either `feature` or `feature=/path`; dependencies may
  be satisfied by another package's provided feature.

### Repository layout

A repository is an HTTPS base URL serving an `index.json`:

```json
{
  "schema_version": 1,
  "packages": [
    {
      "name": "hello",
      "version": "1.0.0",
      "arch": "x86_64",
      "description": "Hello World demonstration package",
      "file": "packages/hello-1.0.0-x86_64.ark",
      "sha256": "<64 hex chars>"
    }
  ]
}
```

`file` is always relative to the repository root and must start with `packages/`.
An entry may also set `download_url` to an absolute HTTPS URL when archives are
hosted elsewhere. Every download is SHA-256 verified against the index and its
embedded `ARKPKG` must match the index entry.

## Archive limits and safety model

`arkpkg` treats archives as untrusted input.

| Limit | Value |
| --- | --- |
| Decoded `ARKPKG` metadata | 1 MiB |
| Decoded tar bytes scanned for metadata | 16 MiB |
| Single archive entry | 2 GiB |
| Total decoded archive | 4 GiB |
| Repository index | 16 MiB |
| Single downloaded archive | 2 GiB |
| Install plan packages / depth | 256 / 128 |
| Install plan total download | 2 GiB |

Extraction only accepts `ARKPKG` and entries under `package/`, rejects absolute
or `..` paths, duplicate entries, symlink ancestors, and non-file/dir/symlink
entries. Installation resolves every parent component inside the target root and
refuses to write through symlinks that escape it; pre-existing files are moved
to sibling temporary backups and restored on failure or removal. Package state
lives in `etc/arkpkg/` (`package.db`, `file.db`, `packages/`, `repos.d/`) and
payload files that would create or replace it are rejected.

Repository publishing is separate: the external `arkpkg-repo` index builder
currently requires `ARKPKG` metadata to be at most 64 KiB, well below the 1 MiB
reader limit.

## Repository state

All state is stored relative to the target root:

```text
etc/arkpkg/
├── package.db          # "<name> <version>" per installed package
├── file.db             # "<installed path>=<owning package>"
├── packages/<name>.arkinfo   # per-package manifest and file digests
└── repos.d/<name>.json       # configured repositories
var/log/arkpkg.log      # append-only transaction log
```

`file.db`, `package.db` and `.arkinfo` are written by atomic replace and the
installation itself is rolled back if any later step fails. Concurrent `arkpkg`
invocations are **not** currently serialised; do not run two installs against the
same root at once.

## Contributing

* Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
  `cargo test --all-targets` before opening a PR; CI runs all three in Docker.
* Add a regression test for every fix that touches archive parsing, path
  resolution, or database writing — most of this code exists to contain hostile
  archives and paths.
* Keep `Dockerfile`, `Cargo.toml` (`rust-version`) and CI in sync when bumping
  the toolchain.

Background reading on the design:
<https://debarkak.xyz/notes/arkanaos-package-manager/>.

## Command-line behaviour notes

* `arkpkg install <name>` prints the full download plan (the requested package
  plus every dependency) and asks for confirmation before it installs anything.
  Use `-y` to skip the prompt in scripts, `-n` to preview and decline.
* Returning a different exit code per failure class (see above) is deliberate:
  scripts can distinguish "not found" from "verification failed" without parsing
  stderr.
* `arkpkg search` prints installed matches first, then repository matches as
  `<name> <version> [<arch>] (<repository>) - <description>`. Repository text is
  control-character-escaped before printing.

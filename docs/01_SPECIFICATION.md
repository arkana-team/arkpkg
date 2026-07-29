# arkpkg Development Specification
Version: 1.0
Status: Draft
Project: arkanaOS
Language: Rust (Stable)

---

# Overview

`arkpkg` is the official package manager for arkanaOS.

It is intentionally designed to be extremely simple, transparent, predictable and Unix-like. Every operation should be understandable by reading plain text files. There should never be any hidden state, binary databases, proprietary formats, or unnecessary complexity.

The philosophy of arkpkg is:

- Keep It Simple.
- Everything should be inspectable.
- Everything should be recoverable.
- Everything should be scriptable.
- Prefer plain text over binary.
- Prefer correctness over cleverness.
- Never sacrifice reliability for convenience.

arkpkg is **NOT** intended to compete with Pacman, APT, DNF, RPM, or APK. Instead, it is designed specifically for arkanaOS and intentionally omits many features found in larger package managers.

There are:

- no repositories
- no mirrors
- no automatic downloads
- no online package indexes
- no package signing (for now)

Packages are simply local `.ark` files.

---

# VERY IMPORTANT

Before writing **ANY** source code, determine every dependency required to build this project.

This includes but is not limited to:

- Rust toolchain
- Cargo
- Docker
- Docker Compose
- build utilities
- required system libraries
- external Rust crates
- development tools
- testing tools
- formatting tools
- linting tools

Examples include:

- rustup
- cargo
- clap
- tar
- lz4
- sha2
- walkdir
- anyhow
- thiserror
- chrono
- tempfile
- serde (only if absolutely necessary)

List every dependency.

Explain why each dependency is required.

After producing the dependency list:

STOP.

Do NOT write any source code.

Wait for confirmation.

Only continue once approval has been given.

This is mandatory.

---

# Development Environment

Development must NEVER occur on the host filesystem.

Everything must happen inside Docker.

The project repository should be mounted into the container.

Inside the container, create a fake installation root.

Example:

/workspace

├── arkpkg/

├── fake_root/

The package manager should install into

/workspace/fake_root

NOT

/

The installation root MUST be configurable.

Never hardcode "/".

The root directory should be configurable via one variable or configuration option.

This allows production to simply switch from

/workspace/fake_root

to

/

without changing the code.

---

# Programming Language

The entire project must be written in Rust.

Requirements:

- Stable Rust only
- Cargo build system
- Rust Edition 2024 (or latest stable if newer)
- No nightly features
- Avoid unsafe unless absolutely required
- Use idiomatic Rust
- Use Result<T, E> properly
- Prefer enums over integer error values internally
- Modular architecture
- Small readable functions
- Comprehensive documentation

---

# Crate Philosophy

Do NOT reinvent solved problems.

Use mature crates whenever appropriate.

Examples:

CLI:
- clap

Archive:
- tar

Compression:
- lz4

Hashing:
- sha2

Filesystem walking:
- walkdir

Temporary directories:
- tempfile

Error handling:
- anyhow
- thiserror

Time:
- chrono

Logging:
- log
- env_logger (or similar)

Do not introduce unnecessary dependencies.

---

# Repository Structure

The repository should eventually resemble:

arkpkg/

├── Cargo.toml

├── Cargo.lock

├── README.md

├── LICENSE

├── SPEC.md

├── Dockerfile

├── docker-compose.yml

├── .gitignore

├── src/

├── tests/

├── examples/

├── scripts/

└── docs/

Every module should have a single responsibility.

Avoid giant source files.

```

---

# Project Layout

```
src/

main.rs

cli.rs

installer.rs

remover.rs

verifier.rs

metadata.rs

archive.rs

checksum.rs

database.rs

logger.rs

package.rs

prompt.rs

version.rs

errors.rs
```

Every module should be independent whenever possible.

No circular dependencies.

---

# Package Format

Packages use the extension:

```
.ark
```

A package is **NOT** a custom archive format.

A package is simply:

```
tar.lz4
```

renamed to

```
.ark
```

Nothing more.

There is:

- no binary header
- no magic bytes
- no proprietary format

If a user renames

```
foo.tar.lz4
```

to

```
foo.ark
```

it should still work.

---

# Package Structure

Every package must contain:

```
ARKPKG

package/
```

Example:

```
bash.ark

ARKPKG

package/

usr/

etc/

var/
```

The installer copies:

```
package/*
```

into the installation root.

Nothing else.

No install scripts.

No pre-install hooks.

No post-install hooks.

No Lua.

No shell execution.

Packages are data only.

---

# Metadata File

The metadata file is named:

```
ARKPKG
```

It is plain text.

Not JSON.

Not YAML.

Not TOML.

Fields:

Name:

Description:

Version:

Arch:

URL:

License:

Maintainer:

Dependencies:

Provides:

Conflicts:

Example:

Name: bash

Description: GNU Bourne Again Shell

Version: 5.3.0

Arch: x86_64

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

---

# Version Operators

Supported operators:

=

==

!=

<

<=

>

>=

Example:

glibc>=2.41

openssl==3.5.0

libfoo!=2.1

Every comparison should behave exactly as expected.

Version parsing should be deterministic.

Never perform lexicographic comparison.

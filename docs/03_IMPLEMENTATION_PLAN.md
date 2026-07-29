---

# Package Builder

Package creation is intentionally separated from package management.

The package manager itself **must not** create packages.

Instead, a second executable should exist:

```
arkpkg-build
```

Its sole responsibility is converting a package directory into a `.ark` archive.

Example:

```bash
arkpkg-build ./bash
```

Output:

```text
Building package...
Compressing...
Writing bash-5.3.0-x86_64.ark
Done.
```

This follows the same philosophy as:

- `pacman` ↔ `makepkg`
- `dpkg` ↔ `dpkg-deb`

Keeping these responsibilities separate keeps the codebase smaller, easier to maintain, and more Unix-like.

---

# Building Packages

A package directory must contain exactly:

```
package-name/

├── ARKPKG
└── package/
```

Example:

```
bash/

├── ARKPKG
└── package/
    ├── usr/
    ├── etc/
    └── var/
```

The output filename should follow this convention:

```
<name>-<version>-<arch>.ark
```

Example:

```
bash-5.3.0-x86_64.ark
```

---

# Package Validation

Before building, validate:

- ARKPKG exists
- package/ exists
- required metadata fields exist
- version format is valid
- architecture is valid
- dependency syntax is valid
- no duplicate fields exist

If validation fails:

Abort immediately.

Never generate an invalid package.

---

# Cargo Configuration

The project should compile with:

```
cargo build
```

Development:

```
cargo run
```

Release:

```
cargo build --release
```

Testing:

```
cargo test
```

Linting:

```
cargo clippy
```

Formatting:

```
cargo fmt
```

The repository should always build successfully on the latest stable Rust release.

---

# Docker

Provide a Dockerfile that contains everything required to develop arkpkg.

The Docker image should include:

- Stable Rust
- Cargo
- Git
- LZ4 utilities
- Tar
- Build tools
- Formatting tools
- Clippy
- Rustfmt

The image should be reproducible.

Do not rely on packages already installed on the host.

---

# Testing

Every major module should include unit tests.

At minimum, test:

Metadata parser

Version comparison

Dependency parser

Checksum generation

Archive extraction

Database parsing

Package installation

Package removal

Verification

File ownership

Prompt handling

Error handling

Tests should never modify the real host filesystem.

Always use temporary directories.

---

# Sample Packages

Provide at least two example packages.

Example:

```
examples/

hello/

bash/
```

The hello package should install a single executable.

The bash package should demonstrate:

- dependencies
- provides
- conflicts
- multiple installed files

These examples serve as both documentation and test data.

---

# Coding Standards

The codebase should follow these principles.

## General

Write clear, boring code.

Avoid clever tricks.

Future contributors should understand the code immediately.

Prefer readability over brevity.

---

# AI Implementation Rules

These rules are mandatory for any AI contributing to this repository.

## Rule 1

Never invent new architecture.

If the specification already defines behavior, follow it exactly.

---

## Rule 2

Do not introduce new dependencies without explaining why they are required.

---

## Rule 3

Do not replace plain text formats with JSON, YAML, TOML, XML, SQL, or binary formats unless explicitly instructed.

---

## Rule 4

Do not silently change CLI behavior.

Every user-facing behavior should match this specification.

---

## Rule 5

Implement one subsystem at a time.

---

## Rule 6

When asked to modify existing code:

Do not rewrite unrelated modules.

Only modify what is necessary.

---

## Rule 7

Keep commits small and focused.

---

## Rule 8

If uncertain about a design decision:

Stop. Explain the uncertainty. Ask for clarification. Never guess.

---

# Acceptance Checklist

Before considering arkpkg complete, verify the following:

☐ Builds successfully on stable Rust
☐ Docker environment works
☐ Installs packages
☐ Removes packages
☐ Verifies packages
☐ Creates .ark packages
☐ Supports dependency checking
☐ Supports conflict checking
☐ Supports version operators
☐ Generates .arkinfo
☐ Updates package.db
☐ Updates file.db
☐ Interactive prompts work
☐ --yes works
☐ --no works
☐ --force works
☐ Logs operations
☐ Returns correct exit codes
☐ Passes all unit tests
☐ No unnecessary dependencies
☐ No unsafe code without justification
☐ Documentation is complete
☐ Example packages build correctly
☐ Code is formatted with cargo fmt
☐ Passes cargo clippy
☐ Passes cargo test

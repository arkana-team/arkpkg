---

# Installation Process

Installing a package should always follow the exact same sequence.

The order of operations is important and must not change.

1. Read the package.
2. Verify that it is a valid `.ark` archive.
3. Decompress the archive.
4. Read the `ARKPKG` metadata.
5. Validate every required metadata field.
6. Ensure the package architecture matches the current system.
7. Check if the package is already installed.
8. Verify dependency requirements.
9. Check for package conflicts.
10. Check for file conflicts.
11. Prompt the user if interaction is required.
12. Copy files into the installation root.
13. Calculate SHA-256 checksums.
14. Generate the `.arkinfo` file.
15. Update `package.db`.
16. Update `file.db`.
17. Write the transaction log.
18. Report success.

If any unrecoverable step fails, installation must stop immediately.

The package manager should never leave the system in an inconsistent state.

Whenever possible, rollback partially installed files.

---

# Removal Process

Removing a package should follow this sequence.

1. Read the package's `.arkinfo`.
2. Check if other installed packages depend on it.
3. Prompt the user if dependency conflicts exist.
4. Delete every file listed in `.arkinfo`.
5. Remove empty directories if they become empty.
6. Remove the package entry from `package.db`.
7. Remove file ownership entries from `file.db`.
8. Delete the `.arkinfo`.
9. Log the transaction.
10. Report success.

Directories must NEVER be recursively deleted.

Only remove a directory if:

- it exists
- it is empty
- it was created solely for the package

System directories such as:

```

/usr
/etc
/bin
/lib
/var

```

must never be removed.

---

# Verification

The verify command should inspect every installed file.

For each file:

- Does the file still exist?
- Does the SHA-256 checksum match?
- Is the file type unchanged?
- (Optional) Are permissions unchanged?

Example output:

```

OK      /usr/bin/bash

OK      /usr/bin/rbash

MODIFIED /etc/bash.bashrc

MISSING /usr/share/man/man1/bash.1.gz

```

Verification should never modify anything.

It is strictly read-only.

---

# Package Database

Installed packages are stored in

```

/etc/arkpkg/package.db

```

The format is intentionally simple.

Example:

```

bash 5.3.0

glibc 2.42

coreutils 9.7

```

One package per line.

Nothing more.

No binary format.

No JSON.

No SQL.

No XML.

Reading the database should require only a few lines of code.

---

# File Ownership Database

Every installed file should also be tracked.

Location:

```

/etc/arkpkg/file.db

```

Example:

```

/usr/bin/bash=bash

/usr/bin/rbash=bash

/etc/bash.bashrc=bash

/usr/share/man/man1/bash.1.gz=bash

```

This database allows extremely fast ownership lookups.

When installing:

Check `file.db` before writing a file.

When removing:

Delete ownership entries immediately.

---

# Package Information Files

Each installed package generates one metadata file.

Location:

```

/etc/arkpkg/packages/

```

Example:

```

bash.arkinfo

```

Contents:

```

Name:

Description:

Version:

Arch:

URL:

License:

Maintainer:

InstalledAt:

Checksum:

Files:

Provides:

```

Example:

```

Name: bash

Version: 5.3.0

Arch: x86_64

InstalledAt: 2026-07-29T13:52:10Z

Checksum:
1cb56...

Files:

/usr/bin/bash

/usr/bin/rbash

/etc/bash.bashrc

Provides:

bash=/usr/bin/bash

sh=/usr/bin/bash

```

The Files section is authoritative.

Never attempt to uninstall files that are not listed here.

---

# Interactive Prompts

arkpkg should be interactive by default.

Whenever user confirmation is required, display a prompt.

Example:

```

The file already exists:

/usr/bin/bash

Owned by package:
bash

Overwrite?

[Y] Yes

[N] No

[A] Yes to All

[S] No to All

```

The user's selection should apply only to the current operation unless "Yes to All" or "No to All" is chosen.

The package manager should never repeatedly ask the same question after the user selects a global answer.

---

# Non-Interactive Operation

For automation and scripting, interactive prompts must be bypassable.

Supported flags:

```

--yes

```

Automatically answer "Yes" to every prompt.

```

--no

```

Automatically answer "No" to every prompt.

```

--force

```

Skip safety checks where appropriate.

Use this option sparingly.

---

# Commands

Supported commands:

```

arkpkg install package.ark

arkpkg remove package

arkpkg verify package

arkpkg info package

arkpkg list

arkpkg search query

```

Repository operations are intentionally NOT implemented.

There are no:

- update
- upgrade
- sync
- download
- mirror

Those features are outside the scope of arkpkg.

---

# Search

Search should inspect:

- package names
- descriptions

Example:

```

arkpkg search shell

```

Output:

```

bash

GNU Bourne Again Shell

```

Search is limited to installed packages.

---

# Logging

Every action should be logged.

Console output should remain minimal.

Example:

```

INFO Reading package

INFO Reading metadata

INFO Checking dependencies

COPY /usr/bin/bash

COPY /usr/bin/rbash

WRITE bash.arkinfo

UPDATE package.db

DONE

```

Avoid:

- colours
- progress bars
- animations
- unnecessary verbosity

Persistent logs should be stored in

```

/var/log/arkpkg.log

```

The log file should be append-only.

---

# Error Handling

Errors should always explain:

- what failed
- why it failed
- how to fix it (if possible)

Bad:

```

Error

```

Good:

```

Dependency missing:

glibc >= 2.41

Please install the required package before continuing.

```

Never display Rust panic messages to users.

Unexpected internal errors should be caught and reported cleanly.

---

# Exit Codes

The following exit codes are reserved.

```

0  Success

1  Generic error

2  Package not found

3  Missing dependency

4  Architecture mismatch

5  Verification failed

6  Package already installed

7  Package conflict

8  Invalid package

9  Permission denied

10 User cancelled operation

```

These exit codes should remain stable to ensure compatibility with shell scripts and automation.

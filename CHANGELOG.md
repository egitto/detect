# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- `-L` / `--follow` flag to follow symlinks and evaluate predicates against their targets. Dangling and looping links are skipped with a warning.

### Changed

- Symlinks are no longer implicitly followed when evaluating predicates: `type == symlink` now matches links, and content/structured predicates don't match them unless `-L` is passed
- `created` now uses the file's birth time instead of inode change time; its `ctime` alias is replaced by `btime`
- Timestamp comparisons use sub-second precision
- Library API: `RuntimeConfig` has a new `follow_links` field (struct literals need `..Default::default()`), `eval::fs::eval` takes a `follow_links` argument, and `TimeMatcher::is_match` takes a `SystemTime`

### Fixed

- Searching a directory containing a FIFO no longer hangs
- Metadata-only queries now match files and directories the user can't read
- Files with pre-1970 or out-of-range timestamps no longer cause a panic
- Output paths no longer render as `././x` or `.//abs/path` for `.`, absolute, or symlinked search roots
- Huge relative times (`modified > -99999999999d`) give a parse error instead of panicking
- Log output goes to stderr instead of being mixed into results on stdout

## [0.3.0] - 2025-01-22

### Added

- **Structured data selectors** for querying YAML, JSON, and TOML file contents
  - Dot notation for nested field access: `yaml:.server.port`
  - Array indexing: `json:.dependencies[0]`
  - Wildcards with OR semantics: `yaml:.features[*].enabled`
  - Recursive descent: `yaml:..field` finds field at any depth
  - Comparison operators: `==`, `!=`, `>`, `<`, `>=`, `<=`
  - String matchers: `contains`, `~=` (regex)
  - Automatic type coercion between numbers and strings
  - Fully composable with other predicates: `size < 50kb AND yaml:.server.port > 8000`
- `--max-structured-size` CLI flag to configure maximum file size for structured parsing (default: 10MB)
- Support for multi-document YAML with OR semantics (matches if ANY document matches)
- **Single-word file type aliases**: Use `file`, `dir`, `symlink`, etc. as shorthand for `type == file`, `type == dir`, etc. Enables natural queries like `dir && depth > 0` or `file && size > 1mb`. All file type values work as aliases (case-insensitive).
- MCP (Model Context Protocol) server support for AI assistant integration
- Better error messages with source location tracking and helpful suggestions
- Unquoted regex pattern support - `content ~= [0-9]+` works without quotes
- Parse-time validation for `type` selector values (breaking change - see below)
- Relative path display in search results
- Dual MIT/Apache-2.0 licensing
- Greater than or equal (`>=`) and less than or equal (`<=`) operators for temporal selectors

### Changed

- Two-phase parser architecture (raw parsing → type checking) for better errors
- Relative time formats now support full aliases (`-7days`, `-2hours`, etc.)

### Breaking Changes

- `type` selector now validates file type values at parse time. Invalid types like `type == dirq` produce parse errors instead of matching nothing. Valid types: `file`, `dir`/`directory`, `symlink`/`link`, `socket`/`sock`, `fifo`/`pipe`, `block`/`blockdev`, `char`/`chardev` (case-insensitive)

# GitHub research tools

Seven independently grantable WebAssembly components adapt the read-only tools in
`5omeOtherGuy/ampi`, `src/extensions/ampi-github/`. They share the portable
`p1-github-guest` crate and a tool-world adapter, not a shell implementation.

| Environment module key / tool name | Package | Arguments (`?` optional) |
|---|---|---|
| `read_github` | `p1/read-github` | `repository`, `path`, `revision?`, `read_range?` |
| `list_directory_github` | `p1/list-directory-github` | `repository`, `path?`, `revision?`, `limit?`, `offset?` |
| `glob_github` | `p1/glob-github` | `repository`, `filePattern`, `revision?`, `limit?`, `offset?` |
| `search_github` | `p1/search-github` | `repository`, `pattern`, `path?`, `limit?`, `offset?` |
| `commit_search` | `p1/commit-search` | `repository`, `query?`, `path?`, `author?`, `since?`, `until?`, `limit?`, `offset?` |
| `diff_github` | `p1/diff-github` | `repository`, `base`, `head`, `path?`, `includePatches?` |
| `list_repositories` | `p1/list-repositories` | `pattern?`, `organization?`, `language?`, `limit?`, `offset?` |

Arguments are strings except integer `limit`/`offset`, boolean `includePatches`, and
`read_range`, an inclusive `[start,end]` pair of positive integer line numbers.
Unknown fields, nulls, fractional limits and invalid ranges are refused before I/O.
`repository` accepts `owner/repo` or `https://github.com/owner/repo`, optionally
ending in `.git`; page URLs are refused rather than losing a revision/path silently.
Branches/tags/SHAs are supported where `revision` is advertised. Code search uses
GitHub's default-branch index, not an arbitrary revision.

## Composition and credentials

These are release host entries. Environments explicitly select them, for example:

```toml
[[tools]]
module = "read_github"
[[tools]]
module = "search_github"
```

A worker can receive these module keys only through the usual host grants. They
do not automatically join every environment. No native tool fallback is present.
Build the packages with `scripts/build-modules.sh --all`; the release pipeline
includes their manifests/components like the other tool packages.

Each component imports only `github-api`, a new tool-class capability. The host
fixes the origin to `https://api.github.com`, the method to GET, and the allowed
endpoints to repository metadata/contents/trees/commits/comparisons, repository/code/
commit searches and accessible-repository enumeration. Redirects are refused.
There are no GitHub write operations, shell access, general URL fetching, guest
headers or guest credential access. Selection of a tool is the network opt-in;
there is no separate ampi enable flag in p1.

The host takes the first nonempty token from `P1_GITHUB_TOKEN`, `AMPI_GITHUB_TOKEN`,
`MMR_GITHUB_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_PERSONAL_ACCESS_TOKEN`, in
that order. It uses the host environment snapshot when supplied and registers the
selected value with the agent's redactor. It does not inspect gh's credential
store. Without a token, public reads work anonymously; code search and private
repository access require a suitable token. Never put a token in tool arguments.

## Pagination and limits

- File reads return numbered UTF-8 text; other results are JSON text with links,
  result data and continuation/completeness metadata. Errors use p1's error status;
  cancellation uses its cancelled status. HTTP errors do not expose response bodies.
- Host requests are bounded to 30 seconds and 8 MiB, including streaming bodies.
  Tool output is bounded to 128 KiB. File `read_range` is applied before the output
  cap; raw reads avoid ampi's inline-base64 size restriction. Binary text is refused.
- Directory and glob results are sorted before paging. Directory limits default to
  100/max 1000, glob to 100/max 300, code/repository search to 30/max 100, commits
  to 50/max 100. Offsets can be arbitrary integers; use the returned `next_offset`.
- GitHub Contents API directories expose at most 1000 entries; use `glob_github`
  for large trees. A truncated recursive tree is refused rather than reported as
  complete. Search exposes at most 1000 results and marks incomplete/capped results.
- Commit query plus path scans successive path-filtered pages, doing literal
  case-insensitive message-substring matching. Its offset counts examined commits,
  not matches. A call scans at most 20 API pages and supplies a continuation when
  more history remains. Query without path uses GitHub commit search syntax.
- Repository discovery pages the token's accessible repositories in name order.
  With filters, it scans up to 20 pages, filters before slicing, and identifies
  incomplete scans and known match counts. The filtered offset pages matches within
  that scanned prefix; it cannot resume scanning beyond the 20-page cap. An
  incomplete result with null `next_offset` means no more known matches, not an
  exhaustive search. Public search is used when unauthenticated
  or a complete filtered scan finds no accessible matches. Unlike ampi, it does not
  combine two independently paginated sources or claim a combined global total.
- Comparisons expose at most 300 changed files and flag that boundary. Path filters
  select one exact file. Patches are limited to 4096 characters each; missing binary/
  large patches are identified explicitly. Search fragments cap at 2048 characters
  and commit messages at 1024, with explicit truncation markers.

All tests use scripted responses or scratch workspaces; none contact GitHub.

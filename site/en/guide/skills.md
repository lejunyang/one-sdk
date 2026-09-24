# Agent skills

`osdk skills` installs **agent skills** — `SKILL.md` instruction packages that AI
coding agents such as Claude Code, Codex, Cursor, OpenCode, Gemini CLI, and
GitHub Copilot read. A skill is content for an external agent, so osdk does only
three things with it: download it from a source, place it in the same BLAKE3
content-addressed store that tools use, and link it into each agent's skills
directory. **osdk never runs a skill's scripts.**

A skill belongs to the same "fetch immutable content, verify, land it, record a
lock, reproduce" family as a model snapshot, and reuses osdk's existing
content-addressed store, link modes, and `osdk.lock`.

## Command reference

```text
osdk skills add <SOURCE>
  [-s|--skill NAME]...        # pick skills by name in a multi-skill repo (* for all)
  [-a|--agent ID]...          # target agents; defaults to [skills].default_agents
  [-g|--global]               # install into the agent's user-level dir, not the project
  [--copy]                    # copy instead of linking
  [--ref REF]                 # GitHub version: branch:main / tag:v1 / rev:<sha> / branch / commit
  [-l|--list]                 # list the skills a source offers, without installing
  [--no-lock]                 # do not record the install in osdk.lock

osdk skills list [-g]
osdk skills find [QUERY...] [--owner OWNER] [--limit N]
osdk skills remove NAME [-g] [-a ID]...
osdk skills sync [-g]
osdk skills update [SKILL...] [-g]
osdk skills path NAME
osdk skills use <SOURCE> [-s NAME] [-a ID] [--ref REF]
osdk skills init [NAME]
osdk skills agents
```

`find` (alias `search`) searches GitHub for installable skill repositories and
prints each hit as `owner/repo`, ready to pass to `add`. It queries GitHub's
public search API anonymously and only falls back to `GITHUB_TOKEN`/`GH_TOKEN`
when it hits the anonymous rate limit; it **never contacts skills.sh**, so no
registry key is needed. `--owner` restricts to one org/user and `--limit` caps
the result count (1–50).

`sync` and `update` are a pair: `sync` reproduces the commit the lock records
(unchanged), while `update` re-resolves a floating ref (a branch/tag, taken from
`[skills.<name>].ref`) to its current commit and rewrites the lock only when it
moved. `use` installs nothing and writes no lock — it uses one skill on the fly:
with no `-a` it writes the generated prompt to stdout (pipe it, e.g.
`osdk skills use owner/repo | claude`), and with `-a <id>` it starts that agent's
CLI interactively with the prompt. `init` scaffolds a `SKILL.md` template so you
can start authoring a skill.

### Source formats

| Form | Example |
| --- | --- |
| GitHub shorthand | `owner/repo` |
| GitHub namespaced | `github:owner/repo` |
| Repo subdirectory | `github:owner/repo/skills/web-design-guidelines` |
| github.com URL | `https://github.com/owner/repo/tree/main/skills/x` (`tree/<ref>` is stripped) |
| Local path | `./my-skills`, `../x`, `/abs/x`, `~/x` |

A source can be a **single skill** (a `SKILL.md` at its root) or a **collection**.
Collection discovery looks in the repo root, its immediate subdirectories, and the
conventional containers `skills/`, `.agents/skills/`, and `.claude/skills/`; use
`-s` to pick by directory name, or `-s '*'` for all.

## Target agents

`osdk skills agents` lists the agents osdk knows and their project / global skills
directories:

| Agent | `--agent` | Project dir | User-level dir |
| --- | --- | --- | --- |
| Claude Code | `claude-code` | `.claude/skills` | `~/.claude/skills` |
| Codex | `codex` | `.agents/skills` | `~/.codex/skills` |
| Cursor | `cursor` | `.agents/skills` | `~/.cursor/skills` |
| OpenCode | `opencode` | `.agents/skills` | `~/.config/opencode/skills` |
| Gemini CLI | `gemini-cli` | `.agents/skills` | `~/.gemini/skills` |
| GitHub Copilot | `github-copilot` | `.agents/skills` | `~/.copilot/skills` |
| Universal | `universal` | `.agents/skills` | `~/.config/agents/skills` |

Several agents share `.agents/skills` on purpose: one install serves them all, and
`remove` counts how many agents still reference a skill. osdk deliberately does not
write into "every detected agent" — pass `-a` explicitly or set
`[skills].default_agents`, or the command errors and tells you how to choose.

## Immutable identity and reproduction

`add` records the identity in `osdk.lock`:

```toml
[skills.web-design-guidelines]
source = "github:vercel-labs/agent-skills/skills/web-design-guidelines"
content_hash = "b3-v2:…"                 # BLAKE3 digest of the staged content
resolved_commit = "063bee94…"            # the immutable commit a floating ref resolved to
agents = ["claude-code"]
```

`osdk skills sync` reproduces from it: it prefers the staged content-addressed
copy, and when that copy is gone it **re-downloads a GitHub source at the recorded
`resolved_commit`** and recomputes `content_hash`. A mismatch fails closed — a
moved tag or a substituted mirror cannot install. A local source whose staged copy
is gone cannot be reproduced and is reported.

## How skills land

The default is a directory link: a junction on Windows, a symlink on Unix (neither
needs privilege). Where links are unavailable, or with `--copy`, the tree is
copied. Either way, osdk **refuses to replace a real directory it did not place**,
so it never deletes your own files. Override the link mode for skills alone with
`[skills].link_mode`.

## Security

- **Fail-closed downloads:** a GitHub tarball goes through osdk's existing download
  stack, bounded by archive size and entry-count limits; a skill's file count
  (default 1000) and total size (default 25 MiB) are capped too, so an oversized or
  hostile repo fails loudly instead of being staged whole.
- **Pre-install preview:** the first install prints the skill's `name`,
  `description`, file count, size, and whether it contains script-like files, so you
  see what an agent will read before it is written into the agent directory.
- **osdk does not execute skills:** staging and linking are osdk's job; execution
  happens inside the downstream agent.
- **Trust gate:** read-only commands (`agents` / `list` / `path`) never trip it;
  `add` / `remove` / `sync` write to disk and stay gated. A declarative `[skills]`
  entry needs no trust to say *what* to install; only a byte-source key such as
  `endpoint` does.

## Declarative configuration

You can also declare skills in `osdk.toml` and let a team reproduce them with
`osdk skills sync`:

```toml
[skills]
default_agents = ["claude-code"]

[skills.web-design]
source = "github:vercel-labs/agent-skills"
skill = "web-design-guidelines"
ref = "branch:main"
agents = ["claude-code", "codex"]
```

The top-level `[skills]` table accepts `default_agents`, `scope`
(`project`/`global`), and `link_mode`; each `[skills.<name>]` accepts `source`,
`skill`, `ref`, `agents`, `when`, and `endpoint`, and a misspelled field errors
loudly. The `osdk-guide` skill's `reference/configuration.md` documents each field.

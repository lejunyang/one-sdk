# Quick Start

The commands below walk through the whole loop, from installing a tool to reproducing it on another machine. See [Install, Switch, and Inspect](../basics/commands) for each command and [Global Command-line Options](../config/cli-options) for shared flags.

```bash
# Install explicit requests; prefixes select the highest matching stable version
osdk install node@20 python@3.12

# Install and write the request to this project
osdk use node@20

# Generate and commit the selected platform's resolution
osdk lock

# Recreate it on another machine of the same platform
osdk install

# Run temporarily without changing a project pin
osdk exec --tool node@20 -- node --version
```

Tool requests normally use `TOOL@VERSION`. Omitting `@VERSION` means `latest`.
Common requests include exact versions (`20.11.1`), prefixes (`20`, `20.11`),
`latest`, `current`, `stable`, `lts`, `lts/iron`, and user aliases. Supported
channels and ranges vary by backend.

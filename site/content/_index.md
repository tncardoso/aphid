+++
title = "Aphid"
description = "A coding agent, built in Rust — fast and hackable."

[params.hero]
eyebrow = "A coding agent, built in Rust"
headline = "Fast and hackable"
subhead = "Every stage — request, stream, tool call, permission prompt — is a plugin hook you can observe, block, or rewrite."
cta_primary_label = "Install Aphid"
cta_primary_href = "#getting-started"
cta_secondary_label = "Read the docs"
cta_secondary_href = "/docs/"

[params.getting_started]
title = "Getting Started"
paragraph = "Clone the repository, install the CLI, and point it at a model. The full book covers the rest — a first alate, project instructions, and adding another provider."
terminal_title = "getting-started — sh"
steps = [
  "git clone https://github.com/tncardoso/aphid",
  "cd aphid",
  "cargo install --path crates/aphid-cli",
  "export DEEPSEEK_API_KEY=sk-...",
  "aphid",
]
cta_docs_label = "Read the docs"
cta_docs_href = "/docs/"
cta_github_label = "View on GitHub"
cta_github_href = "https://github.com/tncardoso/aphid"

[params.footer]
license = "MIT License"
github_label = "github.com/tncardoso/aphid"
github_href = "https://github.com/tncardoso/aphid"
+++

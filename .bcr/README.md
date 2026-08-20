# Bazel Central Registry

Configuration for publishing each tagged release to the
[Bazel Central Registry](https://registry.bazel.build) automatically.

`.github/workflows/release.yaml` cuts the release archive, then hands the tag to
`.github/workflows/publish.yaml`, which opens the registry pull request from
these templates. See
<https://github.com/bazel-contrib/publish-to-bcr/blob/main/templates/README.md>
for authoritative documentation about the file formats.

Publishing needs two things set up once, by hand:

1. A fork of `bazelbuild/bazel-central-registry` under the same owner as this
   repo, named in `publish.yaml`'s `registry_fork`.
2. A repository secret `BCR_PUBLISH_TOKEN` holding a **fine-grained** personal
   access token, with **Repository access** limited to that one fork and
   **Contents: read and write** on it.

A fine-grained token cannot open a pull request against a public repository
([github/roadmap#600](https://github.com/github/roadmap/issues/600)), so
`publish.yaml` sets `open_pull_request: false` and the workflow prints a URL to
open it by hand — one click per release.

The alternative the upstream README documents is a *classic* token with `repo`
and `workflow` scope, which does open the pull request automatically. Classic
tokens cannot be scoped to a single repository: one carries write access to
every repository the account owns, in a public repo's Actions secrets, where
any workflow can read it. The click is worth it. If you want the automation
anyway, do it the way bazel-contrib does — a separate machine account holding
the classic token, so a leak cannot reach anything of yours — and set
`draft: false`, since a bot cannot click "Ready for review".

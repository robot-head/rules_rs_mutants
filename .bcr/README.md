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
2. A repository secret `BCR_PUBLISH_TOKEN` holding a **classic** personal access
   token with `repo` and `workflow` scope. Fine-grained tokens cannot open pull
   requests against public repositories
   ([github/roadmap#600](https://github.com/github/roadmap/issues/600)).

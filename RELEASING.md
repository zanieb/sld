# Releasing sld

* Manually trigger the release workflow to verify that it still works.
* Change version in workspace `Cargo.toml`
* Search for `version = "{old version}"` for other places to update.
* Ensure that the above changes are merged into the main repository.
* Sync local repo to upstream `main`. i.e. no uncommitted changes.
* Run `cargo publish` for each package.
* Trigger the github release action by pushing a tag for the version number.

```shell
git tag 2026.5.20 # Where "2026.5.20" is the number in Cargo.toml
git push origin refs/tags/2026.5.20
```

That should trigger the `release.yml` workflow in GitHub. You can follow its progress in the
Actions tab in GitHub.

When complete, it should create the release in GitHub Releases. Maintainers can then use
`git cliff {previous version}...` as a starting point when editing the associated release notes.

If everything looks good, publish the release.

# Publishing this formula

The tap is `lumberroom/homebrew-lumberroom`, and `Formula/lumberroom.rb` there is what brew
installs. `lumberroom.rb` here is the staging copy, so a version can be reviewed alongside the
release it targets before it goes live.

```bash
brew tap lumberroom/lumberroom
brew install lumberroom
```

## Cutting a new version

1. Push the tag and let `cli-release.yml` publish the four archives and `SHA256SUMS` under
   `https://github.com/lumberroom/lumberroom/releases/download/<tag>/`.
2. Update the four URLs in `lumberroom.rb` and replace each `sha256` with the matching `.tar.gz`
   line from that release's `SHA256SUMS`. Carry no version string anywhere else: `brew audit
   --strict` rejects an explicit `version` when every URL already spells it out.
3. Copy the file into the tap at `Formula/lumberroom.rb`, commit, push.
4. Confirm it, rather than assuming it:
   ```
   brew update-reset "$(brew --repo lumberroom/lumberroom)"
   brew audit --strict --online lumberroom/lumberroom/lumberroom
   brew install lumberroom/lumberroom/lumberroom
   brew test lumberroom/lumberroom/lumberroom
   ```

`brew bump-formula-pr` handles steps 2 and 3 against the tap once you have a tag and the hashes.

## What the test block checks

Both commands run offline. `lumberroom version` answers without a server and exits zero, which is
the reason it exists as a command rather than only as a flag. An unknown command covers dispatch:
a fixed message and exit 1. Neither touches the network, so `brew test` stays honest on a machine
with no lumberroom server anywhere near it.

## The 0.1.0 record

The four archives this formula pins were byte-identical to the ones `v0.1.0-rc.4` produced, from a
different CI run on a different day. `scripts/package-archive.sh` exists for that property, since a
formula pins one sha256 and a build that drifts by a timestamp breaks it.

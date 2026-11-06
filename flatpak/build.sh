#!/usr/bin/env bash
# Build and install the Flatpak for the current user.
#   flatpak/build.sh            release id  io.github.evandro777.FastProcMan        (replaces an installed FastProcMan!)
#   flatpak/build.sh --devel    dev id      io.github.evandro777.FastProcMan.Devel  (installs next to it)
#   flatpak/build.sh --bundle   also write fastprocman.flatpak
# Needs: flatpak-builder, and once: flatpak install flathub org.freedesktop.Sdk//25.08 \
#        org.freedesktop.Sdk.Extension.rust-stable//25.08
set -euo pipefail
cd "$(dirname "$0")"
# CI sets FLATPAK_INSTALLATION=--system: its container has the SDK installed system-wide (updating it there fails
# on the overlay filesystem, so deps are not installed) and has no FUSE.
installation=${FLATPAK_INSTALLATION:---user}
if [ "$installation" = --user ]; then opts=--install-deps-from=flathub; else opts=--disable-rofiles-fuse; fi
manifest=io.github.evandro777.FastProcMan.yml
app=io.github.evandro777.FastProcMan
# Outside the repo (which may live in Dropbox): the build tree and the OSTree cache are gigabytes.
cache=${FASTPROCMAN_FLATPAK_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/fastprocman-flatpak}
devel=0; bundle=0
for arg in "$@"; do
  case "$arg" in
    --devel) devel=1 ;;
    --bundle) bundle=1 ;;
    *) echo "unknown option $arg" >&2; exit 2 ;;
  esac
done

echo ">> vendoring crates (needs network once)"
(cd .. && cargo vendor --locked flatpak/vendor >/dev/null)

if [ "$devel" = 1 ]; then
  app=$app.Devel
  # Same manifest with another app-id; paths are relative to the manifest, so keep it in flatpak/.
  sed "s/^app-id: .*/app-id: $app/" "$manifest" > "$app.yml"
  manifest=$app.yml
fi

echo ">> building $app"
flatpak-builder $installation --install --force-clean $opts \
  --state-dir="$cache/state" "$cache/build-dir" "$manifest"

if [ "$bundle" = 1 ]; then
  # FLATPAK_GPG_KEY (CI, tags only): sign commits and summary; system-wide installs refuse unsigned remotes.
  flatpak-builder $installation $opts --force-clean --repo=repo ${FLATPAK_GPG_KEY:+--gpg-sign=$FLATPAK_GPG_KEY} \
    --state-dir="$cache/state" "$cache/build-dir" "$manifest"
  if [ -f signing-key.b64 ]; then
    # --repo-url + --gpg-keys make the bundle name the remote, so it can follow it for updates.
    base64 -d signing-key.b64 | flatpak build-bundle repo fastprocman.flatpak "$app" \
      --repo-url=https://evandro777.github.io/FastProcMan/ --gpg-keys=-
  else
    # No key yet (new project): pushes and pull requests still build. Tags fail earlier, in the workflow.
    echo ">> WARNING: flatpak/signing-key.b64 is missing; run flatpak/new-signing-key.sh. Bundle is not tied to a remote." >&2
    flatpak build-bundle repo fastprocman.flatpak "$app"
  fi
  echo ">> wrote flatpak/fastprocman.flatpak"
fi
echo ">> run it with: flatpak run $app"

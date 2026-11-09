#!/usr/bin/env sh
# Releases `dev` into `prod` (or a hotfix already on `prod`): runs scripts/ci.sh,
# merges with --no-ff, bumps the version in Cargo.toml / package.json, prepends
# the release to CHANGELOG.md (git-cliff, cliff.toml), writes the release's
# client state for the upgrade test, commits it all as the version (`1.2.3`),
# tags it, and fast-forwards `dev`. Never pushes.
#
# The version comes from the Conventional Commits on `dev` (cliff.toml's [bump]),
# except 1.0.0: going to 1.0.0 is a decision, so it is never automatic.
#   scripts/release.sh            the next version, from the commits
#   scripts/release.sh 1.0.0      that version
#   scripts/release.sh --dry-run  show the version and the changelog only
#
# The workflow's, plus the check of GitHub's tests (ADR 0005). Pushing the tag
# makes Woodpecker mirror it to GitHub, which builds the packages and the
# release (.github/workflows/release.yml).
set -eu
cd "$(dirname "$0")/.."

die() {
    echo "$1" >&2
    exit 1
}
command -v git-cliff >/dev/null 2>&1 || die "git-cliff is missing (brew install git-cliff)"
dry=
next=
for arg in "$@"; do
    case "$arg" in
    --dry-run) dry=1 ;;
    *) next=$arg ;;
    esac
done

branch=$(git branch --show-current)
if [ -z "$dry" ]; then # a dry run only reads: any branch, any tree
    case "$branch" in
    dev | prod) ;; # prod: a hotfix squashed into prod
    *) die "releases start from dev (or prod, for a hotfix)" ;;
    esac
    [ -z "$(git status --porcelain)" ] || die "commit or stash your changes first"
fi
git remote | grep -qx origin && git fetch -q --tags origin

if [ -f Cargo.toml ]; then
    current=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
elif [ -f package.json ]; then
    current=$(node -p "require('./package.json').version") # any indentation
else
    die "no Cargo.toml or package.json: adapt this script"
fi

if [ -z "$next" ]; then
    if git tag --list '[0-9]*.*.*' | grep -q .; then
        next=$(git cliff --bumped-version)
        [ "$next" != 1.0.0 ] || die "1.0.0 is your call: scripts/release.sh 1.0.0"
    else
        next=$current # the first release: the version the manifest already has
    fi
fi
echo "$next" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+' || die "not a version: $next"
git rev-parse -q --verify "refs/tags/$next" >/dev/null && die "$next is already released"

# The changelog section, written now: dev's commits aren't on prod until the merge.
changes=$(mktemp)
git cliff --unreleased --tag "$next" --strip header >"$changes"

if [ -n "$dry" ]; then
    echo "Next version: $next (now $current)"
    cat "$changes"
    exit 0
fi

# macOS, Windows and Linux arm64 are tested on GitHub, from the mirrored dev
# (ADR 0005): its latest test run there must have passed, on a commit of dev
# with nothing since but what GitHub skips (docs-only commits aren't mirrored).
# A hotfix from prod isn't checked: its code isn't on dev.
if [ "$branch" = dev ]; then
    command -v gh >/dev/null 2>&1 || die "gh is missing (brew install gh, gh auth login)"
    run=$(gh run list -R theca-cloud/computer -w test.yml -b dev -L 1 \
        --json status,conclusion,headSha,url --jq '.[0] // empty | "\(.status) \(.conclusion) \(.headSha) \(.url)"')
    case "$run" in
    "completed success "*) ;;
    *) die "GitHub's latest test run on dev didn't pass: ${run:-none}" ;;
    esac
    tested=$(echo "$run" | cut -d' ' -f3)
    git merge-base --is-ancestor "$tested" dev 2>/dev/null || die "GitHub tested $tested, not on dev: push dev, wait for its run"
    untested=$(git diff --name-only "$tested" dev | grep -Ev '\.md$|^LICENSE$|^\.gitignore$|^cliff\.toml$' || true)
    [ -z "$untested" ] || die "dev changed since GitHub's tests ($tested): push dev, wait for its run"
    echo "GitHub's tests passed: $run"
fi
scripts/ci.sh || die "scripts/ci.sh failed: nothing released"

if [ "$branch" = dev ]; then
    git checkout -q prod
    git merge -q --no-ff --no-commit dev >/dev/null || die "merge conflict: resolve it, or git merge --abort"
fi

if [ -f Cargo.toml ]; then
    sed -i.bak "s/^version = \"$current\"$/version = \"$next\"/" Cargo.toml
    rm Cargo.toml.bak
    cargo update -q --workspace
fi
if [ -f package.json ]; then
    npm version "$next" --no-git-tag-version --allow-same-version >/dev/null
fi
if [ -s "$changes" ]; then # empty when only skipped commits (chore, test...)
    [ -f CHANGELOG.md ] || printf '# Changelog\n\n' >CHANGELOG.md
    { head -n 2 CHANGELOG.md; cat "$changes"; echo; tail -n +3 CHANGELOG.md; } >"$changes.new"
    mv "$changes.new" CHANGELOG.md
    git add CHANGELOG.md
fi
rm -f "$changes"

# The release's client state, written by this tree and read back by every later
# one (tests/upgrade.rs): committed with the version, never edited after.
THECA_FILL_OUT="$PWD/tests/upgrade/$next" cargo test -q --test upgrade fill -- --ignored >/dev/null ||
    die "the upgrade test's fill failed: git merge --abort, or git reset --hard, then fix it on dev"
git add "tests/upgrade/$next"

git add -u
git commit -q -m "$next"
git tag -a "$next" -m "$next"
git checkout -q dev
if [ "$branch" = dev ]; then
    git merge -q --ff-only prod
    echo "Released $next. Push: git push origin prod dev $next"
else
    echo "Released $next on prod. Bring it into dev: git merge prod, then push prod, dev and $next"
fi

#!/usr/bin/env bash
# Driver for the composite action in ../action.yml. See docs/GITHUB-ACTION.md.
#
# Trust boundary: every piece of user-controlled text (comment bodies, issue
# titles, branch names) is read from $GITHUB_EVENT_PATH with jq or from `gh`
# output, and only ever reaches a command as a quoted argument. Nothing from
# the event is interpolated into this script by `${{ }}`.
#
#   scripts/action.sh                      run (inside GitHub Actions)
#   scripts/action.sh resolve EVENT FILE   print "<mode> <n>" | "skip <why>" | "error <why>"
set -euo pipefail

ACTION_MODE="${ACTION_MODE:-auto}"
ACTION_TRIGGER="${ACTION_TRIGGER:-@wingman}"
ACTION_ALLOWED="${ACTION_ALLOWED:-OWNER,MEMBER,COLLABORATOR}"
ACTION_MODEL="${ACTION_MODEL:-}"
ACTION_PERMISSION_MODE="${ACTION_PERMISSION_MODE:-auto-edit}"

# Event -> what to do. Pure: event name + payload + the three inputs above.
resolve() {
  jq -r --arg event "$1" --arg mode "$ACTION_MODE" --arg trigger "$ACTION_TRIGGER" \
    --arg allow "$ACTION_ALLOWED" '
    ($allow | split(",") | map(gsub("\\s"; "") | ascii_upcase)) as $ok
    | (if $event == "pull_request" then
         {kind: "review", n: .pull_request.number, pr: true, text: null,
          who: .pull_request.author_association, bot: (.pull_request.user.type == "Bot")}
       elif $event == "issue_comment" then
         {kind: (if .issue.pull_request then "address" else "fix" end), n: .issue.number,
          pr: (.issue.pull_request != null), text: (.comment.body // ""),
          who: .comment.author_association, bot: (.comment.user.type == "Bot")}
       elif $event == "issues" then
         {kind: "fix", n: .issue.number, pr: false,
          text: ((.issue.title // "") + "\n" + (.issue.body // "")),
          who: .issue.author_association, bot: (.issue.user.type == "Bot")}
       else null end) as $e
    | if $e == null then "skip unsupported event \($event)"
      elif $e.bot then "skip actor is a bot"
      elif ($ok | index($e.who // "NONE")) == null then
        "skip author_association \($e.who // "NONE") is not in the allowlist"
      elif $e.text != null and ($e.text | contains($trigger) | not) then
        "skip no \($trigger) mention"
      elif $mode == "auto" then "\($e.kind) \($e.n)"
      elif ($mode == "review" or $mode == "address") and $e.pr then "\($mode) \($e.n)"
      elif $mode == "fix" and ($e.pr | not) then "fix \($e.n)"
      elif ($mode | IN("review", "address", "fix")) then "skip mode \($mode) does not apply to this event"
      else "error unknown mode \($mode) (auto|review|fix|address)" end
  ' "$2"
}

die() { echo "::error::wingman: $*"; exit 1; }

# Push/fetch with the token passed per-command. The checkout must NOT persist
# credentials: .git/config is readable by the agent, the env token is not
# (run_shell strips *_TOKEN / *_API_KEY from child processes).
git_auth() {
  local basic
  basic=$(printf 'x-access-token:%s' "$GH_TOKEN" | base64 | tr -d '\n')
  git -c "http.${GITHUB_SERVER_URL}/.extraheader=AUTHORIZATION: basic ${basic}" "$@"
}

# Run a command, teeing stdout+stderr to $LOG; sets STATUS.
run_logged() {
  STATUS=0
  "$@" 2>&1 | tee "$LOG" || STATUS=$?
}

# Comment on an issue/PR with a headline and the tail of $LOG.
comment_tail() {
  local file="$RUNNER_TEMP/wingman-comment.md"
  {
    printf '%s\n\n<details><summary>Last lines of output</summary>\n\n```text\n' "$2"
    tail -n 40 "$LOG" | sed "s/\`\`\`/'''/g"
    printf '\n```\n</details>\n\n[Workflow run](%s)\n' "$RUN_URL"
  } >"$file"
  gh issue comment "$1" --body-file "$file"
}

commit_changes() {
  git add -A
  git diff --cached --quiet || git commit -q -m "$1"
}

run_review() {
  run_logged wingman review "$1" --comment
  [ "$STATUS" -eq 0 ] || die "review exited $STATUS"
}

run_address() {
  local n=$1 cross head base
  cross=$(gh pr view "$n" --json isCrossRepository --jq .isCrossRepository)
  head=$(gh pr view "$n" --json headRefName --jq .headRefName)
  if [ "$cross" = "true" ]; then
    echo "::notice::wingman: PR #$n is from a fork; not pushing to it."
    return 0
  fi
  git_auth fetch -q --no-tags origin "refs/heads/$head"
  git checkout -q -B "$head" FETCH_HEAD
  base=$(git rev-parse HEAD)
  run_logged wingman pr address "$n"
  if [ "$STATUS" -ne 0 ]; then
    comment_tail "$n" "Wingman could not address this PR (exit $STATUS); nothing was pushed."
    exit "$STATUS"
  fi
  commit_changes "wingman: address feedback on #$n"
  if [ "$(git rev-list --count "$base..HEAD")" -eq 0 ]; then
    echo "::notice::wingman: nothing to push for PR #$n."
    return 0
  fi
  git_auth push -q origin "HEAD:refs/heads/$head"
}

run_fix() {
  local n=$1 branch="wingman/issue-$1" base_ref base title prompt existing
  base_ref=$(jq -r .repository.default_branch "$GITHUB_EVENT_PATH")
  title=$(jq -r '.issue.title // ""' "$GITHUB_EVENT_PATH")
  # Sliced by codepoint in jq (not head -c) so the argv stays valid UTF-8 and
  # well under Linux's 128 KiB per-argument limit.
  prompt=$(jq -r --argjson n "$n" '
    "Resolve GitHub issue #\($n) in this repository. You are running unattended in CI: "
    + "make the smallest change that resolves it, keep the verification gate green, and do not "
    + "commit or push (the workflow does). End with a short summary of what you changed.\n\n"
    + "Issue title: \(.issue.title // "" | .[0:500])\n\nIssue body:\n\(.issue.body // "" | .[0:20000])"
    + (if .comment then "\n\nRequest from @\(.comment.user.login):\n\(.comment.body // "" | .[0:20000])" else "" end)
  ' "$GITHUB_EVENT_PATH")

  git checkout -q -B "$branch"
  base=$(git rev-parse HEAD)
  run_logged wingman --print "$prompt" --mode "$ACTION_PERMISSION_MODE"
  if [ "$STATUS" -ne 0 ]; then
    comment_tail "$n" "Wingman's run for this issue ended red (exit $STATUS), so no PR was opened."
    exit "$STATUS"
  fi
  commit_changes "wingman: resolve #$n"
  if [ "$(git rev-list --count "$base..HEAD")" -eq 0 ]; then
    comment_tail "$n" "Wingman finished green but made no changes, so no PR was opened."
    return 0
  fi
  # ponytail: force-push to our own wingman/issue-<n> namespace; a rerun replaces the last attempt.
  git_auth push -q --force origin "HEAD:refs/heads/$branch"
  existing=$(gh pr list --head "$branch" --state open --json number --jq '.[0].number // empty')
  if [ -n "$existing" ]; then
    gh issue comment "$n" --body "Wingman updated #$existing. [Workflow run]($RUN_URL)"
    return 0
  fi
  gh pr create --base "$base_ref" --head "$branch" --title "wingman: $title" \
    --body "Closes #$n. Opened by Wingman ([workflow run]($RUN_URL)) — review before merging."
}

main() {
  : "${GITHUB_EVENT_PATH:?not running inside GitHub Actions}"
  command -v jq >/dev/null || die "jq is required on the runner"
  command -v gh >/dev/null || die "gh is required on the runner"
  case "$(uname -s)" in
    Linux | Darwin) ;;
    *) die "Linux and macOS runners only (got $(uname -s)). Use runs-on: ubuntu-latest." ;;
  esac

  local out kind arg
  out=$(resolve "$GITHUB_EVENT_NAME" "$GITHUB_EVENT_PATH")
  kind=${out%% *} arg=${out#* }
  case "$kind" in
    skip) echo "::notice::wingman: skipping: $arg"; return 0 ;;
    error) die "$arg" ;;
  esac

  git rev-parse --is-inside-work-tree >/dev/null 2>&1 ||
    die "check out the repository first (actions/checkout with persist-credentials: false)"
  if git config --get-regexp '^http\..*\.extraheader$' >/dev/null 2>&1; then
    die "the checkout persisted credentials into .git/config, where the agent can read them. Set persist-credentials: false on actions/checkout."
  fi

  # Credentials: the key goes to WINGMAN_<PROVIDER>_API_KEY, the one form the
  # config layer reads for any provider; the input var is dropped after.
  [ -n "${ACTION_PROVIDER_API_KEY:-}" ] || die "provider-key is empty (fork PRs get no secrets under pull_request)"
  local provider=${ACTION_MODEL%%/*} upper
  [[ "$ACTION_MODEL" == */* && "$provider" =~ ^[a-z0-9_]+$ ]] ||
    die "model must be provider/model, e.g. anthropic/claude-opus-4-7 (got '$ACTION_MODEL')"
  echo "::add-mask::$ACTION_PROVIDER_API_KEY"
  upper=$(printf '%s' "$provider" | tr '[:lower:]' '[:upper:]') # not ${x^^}: macOS bash 3.2
  export "WINGMAN_${upper}_API_KEY=$ACTION_PROVIDER_API_KEY" WINGMAN_MODEL="$ACTION_MODEL"
  unset ACTION_PROVIDER_API_KEY

  # A fresh global dir: an empty trust store, so the repo's .wingman/config.toml
  # loads untrusted (no hooks, mcp, verify command, provider base_url, mode).
  export WINGMAN_HOME="$RUNNER_TEMP/wingman-home" GH_REPO="$GITHUB_REPOSITORY"
  mkdir -p "$WINGMAN_HOME"
  if [ -n "${ACTION_MAX_USD:-}" ]; then
    [[ "$ACTION_MAX_USD" =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "max-usd must be a number"
    printf '[tokens]\nmax_usd_per_session = %s\n' "$ACTION_MAX_USD" >"$WINGMAN_HOME/config.toml"
  fi
  RUN_URL="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID"
  LOG="$RUNNER_TEMP/wingman.log"

  local version=${ACTION_WINGMAN_VERSION:-}
  [ "$version" = latest ] && version=
  VERSION="$version" WINGMAN_INSTALL_DIR="$RUNNER_TEMP/wingman-bin" \
    sh "$(dirname "$0")/install.sh"
  export PATH="$RUNNER_TEMP/wingman-bin:$PATH"

  # Session logs and the index land in .wingman/; keep them out of commits.
  echo ".wingman/" >>"$(git rev-parse --git-path info/exclude)"
  git config user.name "github-actions[bot]"
  git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

  case "$kind" in
    review) run_review "$arg" ;;
    address) run_address "$arg" ;;
    fix) run_fix "$arg" ;;
  esac
}

if [ "${1:-}" = resolve ]; then
  resolve "$2" "$3"
else
  main
fi

#!/usr/bin/env bash
# Self-check for scripts/action.sh's event -> mode resolution. Needs bash + jq.
#   bash scripts/action-selftest.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0

# check <expected> <event> <json> [VAR=value ...]
check() {
  local want=$1 event=$2 got
  printf '%s' "$3" >"$tmp/event.json"
  shift 3
  got=$(env "$@" bash "$here/action.sh" resolve "$event" "$tmp/event.json")
  if [ "$got" = "$want" ]; then
    echo "ok   $event $* -> $got"
  else
    echo "FAIL $event $*: expected '$want', got '$got'"
    fail=1
  fi
}

pr='{"pull_request":{"number":7,"author_association":"MEMBER","user":{"type":"User"}}}'
pr_outsider='{"pull_request":{"number":7,"author_association":"CONTRIBUTOR","user":{"type":"User"}}}'
pr_comment='{"issue":{"number":9,"pull_request":{"url":"x"}},"comment":{"body":"hey @wingman fix CI","author_association":"COLLABORATOR","user":{"type":"User","login":"a"}}}'
issue_comment='{"issue":{"number":3,"title":"t","body":"b"},"comment":{"body":"@wingman please","author_association":"OWNER","user":{"type":"User","login":"a"}}}'
issue_comment_nomention='{"issue":{"number":3},"comment":{"body":"thanks","author_association":"OWNER","user":{"type":"User","login":"a"}}}'
issue_comment_none='{"issue":{"number":3},"comment":{"body":"@wingman rm -rf","author_association":"NONE","user":{"type":"User","login":"x"}}}'
issue_comment_bot='{"issue":{"number":3},"comment":{"body":"@wingman","author_association":"MEMBER","user":{"type":"Bot","login":"b"}}}'
issue_opened='{"issue":{"number":4,"title":"@wingman add a flag","body":null,"author_association":"MEMBER","user":{"type":"User"}}}'
no_assoc='{"issue":{"number":3},"comment":{"body":"@wingman","user":{"type":"User"}}}'

check "review 7" pull_request "$pr"
check "skip author_association CONTRIBUTOR is not in the allowlist" pull_request "$pr_outsider"
check "address 9" issue_comment "$pr_comment"
check "fix 3" issue_comment "$issue_comment"
check "skip no @wingman mention" issue_comment "$issue_comment_nomention"
check "skip author_association NONE is not in the allowlist" issue_comment "$issue_comment_none"
check "skip author_association NONE is not in the allowlist" issue_comment "$no_assoc"
check "skip actor is a bot" issue_comment "$issue_comment_bot"
check "fix 4" issues "$issue_opened"
check "skip unsupported event push" push '{}'
# Explicit modes, allowlist and trigger inputs.
check "review 9" issue_comment "$pr_comment" ACTION_MODE=review
check "skip mode review does not apply to this event" issue_comment "$issue_comment" ACTION_MODE=review
check "skip mode fix does not apply to this event" pull_request "$pr" ACTION_MODE=fix
check "error unknown mode yolo (auto|review|fix|address)" pull_request "$pr" ACTION_MODE=yolo
check "review 7" pull_request "$pr_outsider" "ACTION_ALLOWED=owner, contributor"
check "skip no !bot mention" issue_comment "$issue_comment" ACTION_TRIGGER=!bot

exit "$fail"

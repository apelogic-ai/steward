#!/bin/sh
set -eu

fail() {
  printf '%s\n' 'Claude Code image state check failed' >&2
  exit 1
}

settings_pattern='"(apiKeyHelper|ANTHROPIC_API_KEY|ANTHROPIC_AUTH_TOKEN|CLAUDE_CODE_OAUTH_TOKEN|AWS_ACCESS_KEY_ID|AWS_SECRET_ACCESS_KEY|AWS_SESSION_TOKEN|AWS_SECURITY_TOKEN)"[[:space:]]*:'
if [ "${1-}" = --settings-files ]; then
  shift
  for settings_file do
    settings_file_status=0
    grep -Eq "$settings_pattern" "$settings_file" || settings_file_status=$?
    case "$settings_file_status" in
      0) exit 42 ;;
      1) ;;
      *) exit 43 ;;
    esac
  done
  exit 0
fi

[ "$#" -eq 2 ] || fail
home=$1
managed_settings=$2

for login_path in \
  "$home/.claude.json" \
  "$home/.claude/.credentials.json" \
  "$home/.credentials.json" \
  "$home/.config/claude" \
  "$managed_settings"
do
  if [ -e "$login_path" ] || [ -L "$login_path" ]; then
    fail
  fi
done

claude_dir="$home/.claude"
if [ ! -d "$claude_dir" ]; then
  exit 0
fi

settings_status=0
find "$claude_dir" -type f -name 'settings*.json' -exec "$0" --settings-files {} + \
  || settings_status=$?
[ "$settings_status" -eq 0 ] || fail

credential_status=0
grep -ERq 'sk-ant-[A-Za-z0-9_-]{20,}|(AKIA|ASIA)[A-Z0-9]{16}' "$claude_dir" \
  || credential_status=$?
case "$credential_status" in
  0) fail ;;
  1) ;;
  *) fail ;;
esac

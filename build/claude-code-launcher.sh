#!/bin/sh
set -eu

fail() {
  printf '%s\n' 'Claude Code configuration failed' >&2
  exit 125
}

validate_port() {
  case "$1" in
    ''|*[!0-9]*) fail ;;
  esac
  [ "$1" -ge 1 ] 2>/dev/null || fail
  [ "$1" -le 65535 ] 2>/dev/null || fail
}

base_url=${ANTHROPIC_BASE_URL-}
case "$base_url" in
  http://*|https://*) ;;
  *) fail ;;
esac
case "$base_url" in
  *[[:space:][:cntrl:]]*|*'?'*|*'#'*) fail ;;
esac
authority=${base_url#*://}
authority=${authority%%/*}
case "$authority" in
  ''|*@*) fail ;;
  \[*\]*)
    host=${authority#\[}
    host=${host%%\]*}
    [ -n "$host" ] || fail
    suffix=${authority#*\]}
    case "$suffix" in
      '') ;;
      :*) validate_port "${suffix#:}" ;;
      *) fail ;;
    esac
    ;;
  *:*)
    host=${authority%:*}
    port=${authority##*:}
    case "$host" in
      ''|*:*) fail ;;
    esac
    validate_port "$port"
    ;;
  *'['*|*']'*) fail ;;
esac

[ "${ANTHROPIC_API_KEY-}" = openshell-token-grant-placeholder ] || fail
[ "${DISABLE_AUTOUPDATER-}" = 1 ] || fail
[ "${CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC-}" = 1 ] || fail

for variable in \
  ANTHROPIC_AUTH_TOKEN \
  CLAUDE_CODE_OAUTH_TOKEN \
  CLAUDE_CODE_OAUTH_REFRESH_TOKEN \
  ANTHROPIC_CUSTOM_HEADERS \
  CLAUDE_CODE_USE_BEDROCK \
  CLAUDE_CODE_USE_VERTEX \
  CLAUDE_CODE_USE_FOUNDRY \
  CLAUDE_CODE_USE_MANTLE \
  CLAUDE_CODE_USE_ANTHROPIC_AWS
do
  eval "is_set=\${${variable}+yes}"
  [ "${is_set-}" != yes ] || fail
done

exec /usr/bin/claude "$@"

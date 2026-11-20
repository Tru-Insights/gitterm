#!/bin/sh
# Synthetic gh only. Any unexpected command fails closed.
if [ "$*" = --version ]; then exit 0; fi
here=${0%/*}
printf '%s|%s|%s\n' "${GH_TOKEN-}" "${GITHUB_TOKEN-}" "$*" >> "$here/calls"
case "$*" in
  'auth token --user alice') printf 'fixture-alice\n'; exit 0 ;;
  'auth token --user bob') printf 'fixture-bob\n'; exit 0 ;;
  'auth token --user failed') printf 'fixture token failure\n' >&2; exit 1 ;;
  'auth token --user empty') exit 0 ;;
  'pr review 7 --repo acme/api --approve'|'pr merge 7 --repo acme/api --squash'|'pr ready 7 --repo acme/api') printf 'done\n'; exit 0 ;;
  'pr ready 99 --repo acme/api') printf '%s\n' "$$" > "$here/pid"; exec /bin/sleep 30 ;;
  'api -i -H Accept: application/vnd.github+json user') printf 'HTTP/2 200 OK\r\n\r\n{"login":"active"}'; exit 0 ;;
  *'repos/denied/api/pulls/7') printf 'HTTP/2 403 Forbidden\r\n\r\n{"message":"fixture denied"}'; exit 1 ;;
  *'If-None-Match: "fixture-etag"'*'/pulls/7') printf 'HTTP/2 304 Not Modified\r\n\r\n'; exit 0 ;;
  'api -i -H Accept: application/vnd.github+json repos/'*'/pulls/7') printf 'HTTP/2 200 OK\r\nETag: "fixture-etag"\r\n\r\n{"title":"Fixture","state":"closed","head":{"repo":{"full_name":"acme/api"}}}'; exit 0 ;;
esac
printf 'unexpected fixture command: %s\n' "$*" >&2
exit 97

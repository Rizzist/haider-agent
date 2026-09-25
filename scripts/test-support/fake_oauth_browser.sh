#!/bin/sh
# $BROWSER stand-in for fake-OAuth e2e runs (TEST SUPPORT ONLY).
# Completes the authorize hop by following the fake server's 302 to the
# daemon's loopback callback. It REFUSES any URL that is not on the fake
# origin, so a live provider URL is never fetched.
#   HAIDER_TEST_FAKE_OAUTH_ORIGIN  the fake server origin (required)
#   HAIDER_FAKE_BROWSER_LOG        optional file: one redacted line per URL
url=$1
origin=${HAIDER_TEST_FAKE_OAUTH_ORIGIN:?fake origin required}
log=${HAIDER_FAKE_BROWSER_LOG:-/dev/null}
case "$url" in
  "$origin"/oauth/authorize\?*) ;;
  *) printf 'REFUSED %s\n' "${url%%\?*}" >> "$log"; exit 1 ;;
esac
printf 'FOLLOW %s\n' "${url%%\?*}" >> "$log"
# -L follows the 302 to http://localhost:<port>/callback (the daemon).
curl -sS -L --max-redirs 2 --max-time 20 -o /dev/null -w 'callback_status=%{http_code}\n' "$url" >> "$log" 2>&1

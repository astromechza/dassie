#!/usr/bin/env bash
# Known-answer tests for guest/net-env.sh: addresses, MAC and TAP name at the
# edges of each byte of the /16, plus rejected indices. Runs anywhere.
#
#   guest/net-env-test.sh

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
failed=0

# expect INDEX TAP HOST_IP GUEST_IP MAC
expect() {
  local want got
  want=$(printf 'DASSIE_INDEX=%s\nDASSIE_TAP=%s\nDASSIE_HOST_IP=%s\nDASSIE_GUEST_IP=%s\nDASSIE_MAC=%s\nDASSIE_DNS=1.1.1.1,9.9.9.9' "$@")
  got=$(DASSIE_DNS='' "$here/net-env.sh" "$1")
  if [[ "$got" != "$want" ]]; then
    printf 'FAIL: index %s\n--- want\n%s\n--- got\n%s\n' "$1" "$want" "$got" >&2
    failed=1
  fi
}

expect 0 dt00000 10.88.0.0 10.88.0.1 06:00:0a:58:00:01
expect 5 dt00005 10.88.0.10 10.88.0.11 06:00:0a:58:00:0b
expect 127 dt00127 10.88.0.254 10.88.0.255 06:00:0a:58:00:ff
expect 128 dt00128 10.88.1.0 10.88.1.1 06:00:0a:58:01:01
expect 32767 dt32767 10.88.255.254 10.88.255.255 06:00:0a:58:ff:ff

if [[ "$(DASSIE_DNS=192.0.2.53 "$here/net-env.sh" 1 | tail -1)" != DASSIE_DNS=192.0.2.53 ]]; then
  echo "FAIL: DASSIE_DNS override" >&2
  failed=1
fi

for bad in -1 32768 01 1x "" "1 2"; do
  if "$here/net-env.sh" "$bad" >/dev/null 2>&1; then
    echo "FAIL: index ${bad@Q} accepted" >&2
    failed=1
  fi
done
for bad in 'a b' '1.1.1.1,' 1.1.1.1,2.2.2.2,3.3.3.3,4.4.4.4; do
  if DASSIE_DNS=$bad "$here/net-env.sh" 1 >/dev/null 2>&1; then
    echo "FAIL: DASSIE_DNS=${bad@Q} accepted" >&2
    failed=1
  fi
done
if "$here/net-env.sh" >/dev/null 2>&1; then
  echo "FAIL: no index accepted" >&2
  failed=1
fi

((failed == 0)) && echo "net-env: all tests passed"
exit "$failed"

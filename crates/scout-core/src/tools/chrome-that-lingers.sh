#!/bin/sh
# A stand-in for Chrome, for the tests of what a render leaves behind
# (`tools/browser.rs`). It is what makes a Chrome hard to clean up after and
# nothing else: a second process that goes on writing into the profile after
# the first one is dead, putting the directory back if it has been removed.
#
# Started as Chrome is, it finds its profile in `--user-data-dir=` and takes
# the last argument for the address. An address ending in `/prints` gets a
# page printed, as `--dump-dom` prints one; any other gets nothing, like a
# page that never loads. Either way the first process then waits, as Chrome
# lingers after its page.
profile=
for arg in "$@"; do
  case "$arg" in
    --user-data-dir=*) profile="${arg#--user-data-dir=}" ;;
  esac
  address="$arg"
done
[ -n "$profile" ] || exit 2

(
  n=0
  # Ten seconds or so, far past any test here; then it stops by itself.
  while [ "$n" -lt 400 ]; do
    mkdir -p "$profile/Default"
    : > "$profile/Default/written-$n"
    n=$((n + 1))
    sleep 0.02
  done
) > /dev/null 2>&1 &

case "$address" in
  */prints) echo '<html><body>printed</body></html>' ;;
esac
wait

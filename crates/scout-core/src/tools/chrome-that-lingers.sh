#!/bin/sh
# A stand-in for Chrome, for the tests of what a render leaves behind
# (`tools/browser.rs`). It is what makes a Chrome hard to clean up after and
# nothing else: a second process that goes on writing into the profile after
# the first one is dead, putting the directory back if it has been removed.
#
# Started as Chrome is, it finds its profile in `--user-data-dir=` and takes
# the last argument for the address. An address ending in `/prints` gets a
# page printed, as `--dump-dom` prints one, which also says whether the
# stand-in was given nothing to read; any other gets nothing, like a page
# that never loads. Either way the first process then waits, as Chrome
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
    # Not once the directory the profile was made in is gone: that is the
    # test's own, removed when the test is over, and a writer that had
    # outlived a failing test would put it back for nobody. Two plain
    # `mkdir`s for the same reason: `-p` would make that directory too.
    [ -d "${profile%/*}" ] || exit 0
    mkdir "$profile" 2> /dev/null
    mkdir "$profile/Default" 2> /dev/null
    # `true`, not `:`. The directory can be removed between the line above
    # and this one, and a redirection that fails on `:`, a special built-in,
    # ends the shell: the writer died of the very removal it is here to
    # outlast (found in review, 2026-11-22).
    true > "$profile/Default/written-$n"
    n=$((n + 1))
    sleep 0.02
  done
) > /dev/null 2>&1 &

case "$address" in
  */prints)
    reads=something
    [ /dev/stdin -ef /dev/null ] && reads=nothing
    echo "<html><body>printed, reads $reads</body></html>"
    ;;
esac
wait

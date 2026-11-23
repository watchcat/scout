# Sourced by deploy-k3s.sh. Says whether the pod runs the commit a deploy
# was named for, from what its metrics page says.
#
# A deploy names its image `scout:<commit>`, but the name proves nothing
# about what is inside: a build of a commit older than the node's last
# build used to compile nothing and leave that build's binary under the
# new name. The binary itself says which commit it was compiled from
# (`scout_api::commit`), on the metrics page, as
# `scout_build_info{commit="<commit>"} 1`.
#
#   shipped_problem "$metrics_page" "$commit"
#
# Prints nothing and returns 0 when the page names the commit; prints the
# reason and returns 1 otherwise, an unnamed commit included: this script
# builds every image it deploys with the commit named, so an image that
# names none was built some other way.
shipped_problem() {
    local page=$1 commit=$2 line named
    line=$(grep -m1 '^scout_build_info{' <<<"$page") || true
    case "$line" in
        "scout_build_info{commit=\"$commit\"} 1") return 0 ;;
        'scout_build_info{commit=""} 1')
            echo "the binary the pod runs names no commit"; return 1 ;;
        'scout_build_info{commit="'*)
            named=${line#scout_build_info\{commit=\"}
            echo "the pod runs ${named%%\"*}, not $commit" | cut -c1-120; return 1 ;;
        "") echo "the pod's metrics have no scout_build_info: ${page:0:80}"; return 1 ;;
        *) echo "the pod's metrics say: ${line:0:120}"; return 1 ;;
    esac
}

#   shipped_check "$commit" <fetch> [tries] [pause]
#
# Asks <fetch>, a command that prints the pod's metrics page, and judges
# its answer with shipped_problem. A definite answer is final, as it will
# not change by asking again; a fetch that fails is tried again, `tries`
# times in all (3), `pause` seconds apart (5). Returns 0 when the pod runs
# the commit, 1 when it runs another or names none, and 2 when it could
# not be asked; the reason is printed for both.
shipped_check() {
    local commit=$1 fetch=$2 tries=${3:-3} pause=${4:-5} page try
    for ((try = 1; ; try++)); do
        if page=$("$fetch" 2>&1); then
            shipped_problem "$page" "$commit"
            return
        fi
        if [ "$try" -ge "$tries" ]; then
            echo "the pod could not be asked: ${page:0:200}"
            return 2
        fi
        sleep "$pause"
    done
}

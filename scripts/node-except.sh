# Sourced by deploy-k3s.sh. The node's own addresses, as the browser pod's
# fence excepts them.
#
# The fence lets the pod out to the public internet, private ranges
# excepted, and the node has a public address of its own on which SSH and
# the Kubernetes API answer (measured 2026-11-20). So every address the node
# has goes in the fence's exceptions too. They are read from the node at
# each deploy, never written in a file of this repository, and never
# printed.
#
#   node_except "$(ip -4 -o addr show scope global)" "$internal_ip"
#
# Prints the addresses as the fence's list wants them, "a/32, b/32", and
# returns 0; or prints why not and returns 1. The node's InternalIPs, the
# addresses its API is served on, are given as the cluster says them,
# separated by spaces: an IPv4 and an IPv6 one on this node. Each IPv4 one
# must be among the listed: a listing that does not hold it is not this
# node's, or not all of it. (The pods have no IPv6, so the fence needs none.)
node_except() {
    local listing=$1 internal=$2 list="" address cidr ipv4=0
    while read -r _ _ family cidr _; do
        [ -n "${family:-}" ] || continue
        if [ "$family" != inet ]; then
            echo "a line of the listing is not an IPv4 address's: $family"
            return 1
        fi
        address=${cidr%%/*}
        if ! [[ $address =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]]; then
            echo "a line of the listing has no IPv4 address in it"
            return 1
        fi
        list="${list:+$list, }$address/32"
    done <<<"$listing"
    if [ -z "$list" ]; then
        echo "the node listed no address"
        return 1
    fi
    while read -r address; do
        [[ $address =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || continue
        ipv4=$((ipv4 + 1))
        case ", $list, " in
            *", $address/32, "*) ;;
            *) echo "the node's InternalIP is not among the addresses it listed"; return 1 ;;
        esac
    done < <(tr ' ' '\n' <<<"$internal")
    if [ "$ipv4" -eq 0 ]; then
        echo "the node's InternalIP could not be read"
        return 1
    fi
    echo "$list"
}

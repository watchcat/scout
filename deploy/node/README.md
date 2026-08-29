# Node configuration

Configuration that lives on the k3s node rather than in the cluster. Nothing
here is applied by `scripts/deploy-k3s.sh` — it is set up once, by hand, and
kept here so the node can be rebuilt from the repository.

## `daemon.json` — keep the build cache between deploys

Copy to `/etc/docker/daemon.json` and `systemctl restart docker`.

The restart is safe: k3s runs pods on its own embedded containerd, not on
dockerd. `kubectl get node -o jsonpath='{..containerRuntimeVersion}'` reports
`containerd://…-k3s2`, and `docker ps` on this node is empty. Docker is a
build tool here and nothing else.

### Why

The Dockerfile keeps cargo's registry and `/app/target` in BuildKit cache
mounts, so a deploy should recompile only the crates that changed. Measured
across three deploys:

| deploy    | crates compiled | crates downloaded |
| --------- | --------------- | ----------------- |
| `32aaa76` | 0               | 0                 |
| `c863a47` | 4               | 0                 |
| `9425e19` | 282             | 685               |

The first two ran minutes apart and cost seconds. The third ran four days
later and rebuilt everything, DuckDB's C++ included — roughly ten minutes.

`docker buildx du --verbose` showed why: both cache-mount records were
created at the instant that third build started, meaning the previous ones
were gone. Docker's default BuildKit GC policy reclaims `exec.cachemount`
records **unused for 48 hours**. Ordinary layer cache survives under later,
more generous rules, which is why 6.6 GB of build cache was still present
while the part that mattered had been collected.

So the real behaviour is not "the first build is slow". It is "any deploy
more than two days after the previous one is slow", which for a project that
ships in bursts is most of them.

The policy below replaces the defaults with a single size-bounded rule: keep
build cache until it reaches 25 GB, regardless of age. The node has ~85 GB
free, so this trades disk that is not otherwise doing anything for the ten
minutes back.

`reservedSpace` is the Docker 26+ spelling. The older `defaultKeepStorage` is
deprecated and ignored on this node's Docker 29.7.2.

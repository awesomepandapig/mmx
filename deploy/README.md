# Deploying MMX

Linux x86 is the first-party target, and images are also built for arm64.

| Path | Holds |
|---|---|
| `docker/` | The Dockerfile, Bake file and entry scripts |
| `kustomize/` | A base, a `local` overlay, the UI as a component, and the QuestDB rebuild job |
| `helm/mmx/` | The chart, with `values.yaml` for production and `values-local.yaml` for a laptop or CI |
| `scripts/validate.sh` | Static checks, with no cluster or containers |
| `scripts/local-images.sh` | Image builds with Apple `container` |

**None of this has been run yet.** The manifests, chart and scripts pass static checks only. No image
has been built, and nothing has been installed on a cluster. See [First real run](#first-real-run).

## Images

All images come from one Dockerfile, built from the repository root:

```sh
docker buildx bake -f deploy/docker/docker-bake.hcl                                # everything
PLATFORMS=linux/amd64 docker buildx bake -f deploy/docker/docker-bake.hcl core --load
REGISTRY=registry.example/mmx TAG=0.1.0 docker buildx bake -f deploy/docker/docker-bake.hcl --push
```

| Image | Contents |
|---|---|
| `node` | JRE and `aeron-all-1.54.0.jar`, as a cluster member or, with `driver`, a media driver |
| `matching-engine` | The engine in the `dist` profile with `silent`, as benchmarked |
| `api`, `websocket`, `market-data`, `trade-reporter`, `outbox`, `observer` | One Rust service each |
| `mmx` | `mmx serve` and the CLI, with the console built in. It needs no Aeron and no media driver |
| `tools` | Every `sbx-*` tool, for debugging a running cluster |
| `web`, `docs` | The Remix apps. `VITE_` values are baked in, so set them when building |
| `auth` | Built for now, but not in the chart, because it's being retired |

**Rebuilds.** A source change only recompiles the workspace, because dependencies are in their own
layer, keyed on a recipe that only changes with `Cargo.lock` and the manifests. The whole workspace
builds once per architecture, so all the service images share one Cargo run. The engine is separate,
and its fat LTO link runs on every engine rebuild. BuildKit cache mounts hold the crate index,
downloaded crates, Gradle and rustc's incremental output.

**CPU baseline.** Aeron's C code would otherwise be built with `-march=native`, so the images set
`x86-64-v3` on amd64 and `armv8.2-a` on arm64. To run on older CPUs, pass `AMD64_MARCH` or `ARM64_MARCH`
as build args.

## Preparing nodes

Cluster members need their own nodes, so you need at least three. On each one:

```yaml
# kubelet configuration: Guaranteed pods with whole CPUs get exclusive cores
cpuManagerPolicy: static
cpuManagerPolicyOptions:
  full-pcpus-only: "true"
reservedSystemCPUs: "0,1"
```

```sh
# Aeron's 4m socket buffers are capped by these, which pods can't set for themselves
sysctl -w net.core.rmem_max=8388608 net.core.wmem_max=8388608 vm.swappiness=0
```

Keep them in `/etc/sysctl.d`. `full-pcpus-only` rejects requests that aren't a multiple of the SMT
level, so the defaults of 6 and 2 CPUs suit two threads per core. Without static CPU management, set
`cluster.guaranteed` and `cluster.pinThreads` to false, or pinned threads will share CPUs with other
pods.

A NetworkPolicy does nothing without a CNI that enforces it, so check yours before relying on it.

## Installing

```sh
helm install mmx deploy/helm/mmx --namespace mmx --create-namespace --set image.tag=0.1.0
helm test mmx --namespace mmx     # sends an order through a gateway
kubectl logs --namespace mmx --selector app.kubernetes.io/component=console | grep 'first-run'
kubectl port-forward --namespace mmx service/mmx-console 8093:8093
```

Open <http://localhost:8093>, sign in with the logged password and choose a real one. See
[The console](#the-console). A port-forward is fine for this first step, and exposing it properly is
up to you.

With Kustomize, create an overlay that sets the image tag, like `kustomize/overlays/local`, and run
`kubectl apply -k` on it. The base is one stack per namespace, with three members.

| Value | Default | Meaning |
|---|---|---|
| `ui.enabled` | `true` | The web UI |
| `console.replicas` | `2` | `mmx serve`. Replicas share everything through the Kubernetes API |
| `console.adminPasswordSecret` | Empty | A Secret holding the first password. Empty generates one |
| `marketsYaml`, `webhookUrl` | Empty | Seeds for the first install only. The console owns them after that |
| `demo.enabled` | `false` | Demo mode for the web UI, with a generated password in the `demo` Secret |
| `demo.bots.enabled` | `false` | Load bots, which need a `demo-bots` image |
| `networkPolicy.gameServers` | Pods labeled `mmx.io/role=game-server` | Who may reach gateways |
| `cluster.leaderHeartbeatTimeout` | `500ms` | How fast a leader is replaced, along with `electionTimeout` |
| `cluster.archiveSyncLevel` | `0` | Replication only. 1 or 2 syncs to disk on every write |
| `cluster.engine.maxOrders` | `1000000` | Sets the engine's memory, as shown in `STARTUP.md` |
| `cluster.persistence.size` | `200Gi` | The tape is kept forever by default |
| `secrets.existingSecret` | Empty | A plain Kubernetes secret you create, loaded as environment variables |

Secrets never go in a values file. Create `existingSecret` yourself, with `WS_PLAYER_TOKEN_SECRET` in
it, which must be at least 32 bytes or `websocket` turns off the user channel. The admin token is
generated for you, and the webhook's signing secret comes from the console.

Expose gateways however suits your network, through `gateway.service`, then add your game servers to
`networkPolicy.gameServers`. Pods are one option, and an `ipBlock` works for servers outside the cluster.
The console's pods can always reach gateways, and they're the only thing that can reach the outbox's
status port.

## Ports and TLS

The chart creates Services and no Ingress, so what's exposed, and how, is your choice. Every binary
speaks plain HTTP. **TLS is your job**: terminate it in front of anything that leaves the cluster.

| Service | Port | Who needs it | Exposing it |
|---|---|---|---|
| `mmx-console` | 8093 | Operators, for the console and CLI | Behind TLS, and ideally a VPN or an allow list |
| `mmx-gateway` | 8000 | Game servers only | Never to players: it trusts `x-player-id` |
| `mmx-websocket` | 8080 | Game clients | Behind TLS |
| `mmx-web` | 3000 | Traders, if `ui.enabled` | Behind TLS |
| `mmx-outbox` | 8092 | `mmx serve` only | Never: it has no auth |

When the console is behind TLS, set its public URL in Settings to the `https://` address. Session
cookies are only marked `Secure` once that's set, or if your proxy sends `X-Forwarded-Proto: https`.
A proxy must allow long-lived responses on `/admin/v1/events`, which Activity reads as a stream.

## The console

`mmx serve` is the control plane, and the console is built into it, so there's no extra pod. The CLI
uses the same `/admin/v1` API with `MMX_ADMIN_TOKEN`, which the chart generates once and keeps across
upgrades in the `mmx-control` Secret. Point the CLI at a port-forward:

```sh
export MMX_URL=http://127.0.0.1:8093
export MMX_ADMIN_TOKEN=$(kubectl get secret mmx-control --namespace mmx \
  -o jsonpath='{.data.MMX_ADMIN_TOKEN}' | base64 -d)
```

**First password.** There's no default. The first start reads the Secret in
`console.adminPasswordSecret`, or generates a password and logs it once. With two replicas it's in the
log of whichever got there first, and the selector above reads both. It only opens first-run setup.
Only a hash is stored. `mmx auth reset-password` issues a new temporary one if the password is lost.

**What it stores.** One ConfigMap, `mmx-config`, and one Secret, `mmx-control`, through a ServiceAccount
that can only `get` and `patch` those two by name. Writes are conditional, so a `kubectl apply` in
between is a conflict and not a lost edit. Both carry `helm.sh/resource-policy: keep`, so uninstalling
leaves them, and the next install adopts them. To start again, delete both by hand.

**Markets.** The console owns markets after install. `marketsYaml` seeds the ConfigMap only if it's
missing, using `lookup`, so `helm upgrade` can't overwrite edits. `lookup` is empty under `helm template`
and client-side dry runs, so tools built on those, such as Argo CD, show the seed and a fresh token
instead. Use `kubectl apply` or the console for markets there, and don't let them manage these two
objects.

An applied ConfigMap shows up in the console. The kubelet takes up to about a minute to deliver any
change to gateways, so a save reads "saved, reaching gateways" until they have it. `mmx serve` checks
every 5 seconds and reloads any gateway that differs. Halts live in Valkey, apart from the file, so a
reload can't lift one.

**Webhooks.** Set the URL and generate a signing secret in the console. The secret is shown once. The
outbox reads both at start, and waits without them, so restart it to apply a change. The console shows
whether that's still needed, and the exact command:

```sh
kubectl rollout restart statefulset/mmx-outbox --namespace mmx
```

On a first setup it then delivers the whole tape so far, so expect a large first batch.

**Single sign-on.** Configure an OpenID Connect provider under Settings, with an issuer, client ID,
client secret and a list of who may sign in. Register the redirect URI it shows. The list is required
and everyone on it is a full admin. Emails only count if the provider marks them verified.
Turning off password login needs a sign-in through the provider first, and `mmx auth password-login on`
turns it back on. Sign-on is for the console: gateways don't use it.

The Kustomize base doesn't include `mmx`, because only the chart has been brought up to date.

## How the pods fit together

A cluster pod runs `node` and `matching-engine`, sharing `/dev/shm` through a memory-backed `emptyDir`.
`mmx-node` waits for its peers' names, then starts Java. `mmx-entrypoint` waits for `cnc.dat`, then
starts the engine. Their memory limits must cover `/dev/shm`, because tmpfs pages count against
whichever container touches them first.

Members use the same ports as `tools/scripts/cluster.py`, offset by member ID: ingress `20000+i`,
consensus `20010+i`, log `20020+i`, catchup `20030+i`, log control `20040+i`, Archive `8010+i` and
reports `40450+i`. Gateways and consumers run a media driver beside them, in SHARED mode.

With pinning on, consensus, sender and receiver each take one CPU of the node container, and the rest
of Java shares what's left. The engine's main thread takes one CPU of the engine container, and the
Aeron client's threads share the other.

## Updating cluster members

Members use `OnDelete`, because a pod reports ready before it has caught up with the log, so a rolling
update could take out a quorum. After upgrading, restart them one at a time, and leave the leader
until last:

```sh
kubectl logs mmx-node-0 --namespace mmx -c engine | grep '^role' | tail -1      # find the leader
kubectl delete pod mmx-node-1 --namespace mmx
```

Wait for a member to catch up before moving to the next. There's no status command for that yet, so
this needs the `mmx` CLI or a look at the logs.

## Rebuilding QuestDB

`trade-reporter --rebuild` drops every table each time it starts, then follows the tape forever. So
it's a one-off Job that no install or upgrade creates. It can't retry, and it never finishes by
itself. You stop it, and the normal service carries on from the saved position.

```sh
kubectl scale deployment mmx-trade-reporter --namespace mmx --replicas=0   # two writers duplicate rows

helm template mmx deploy/helm/mmx --namespace mmx --set questdbRebuild.enabled=true \
  --show-only templates/questdb-rebuild.yaml | kubectl apply --namespace mmx -f -
# Or: kubectl apply --namespace mmx -k deploy/kustomize/jobs/questdb-rebuild

kubectl logs job/mmx-questdb-rebuild --namespace mmx -c trade-reporter --follow
# Stop whenever it's running well. Deleting it loses nothing, because rows and position save together.
kubectl delete job mmx-questdb-rebuild --namespace mmx
kubectl scale deployment mmx-trade-reporter --namespace mmx --replicas=1
```

Pass the same values to `helm template` that the release uses, including `-f` and `image.tag`. The
chart refuses `questdbRebuild.enabled` on `helm install` and `helm upgrade`. Don't reapply the Job
unless you mean to drop the tables again.

## Local cluster

This uses `container cluster`, which is experimental and needs Apple `container`. Members can't share
a node, so it needs three workers. The control plane gets the same size as a worker, so this takes
12 CPUs and 20 GB of memory, which can be shared with the host.

```sh
container cluster create --name mmx --workers 3 --cpus 3 --memory 5g --shared-image-store
kubectl config use-context mmx
kubectl get nodes

deploy/scripts/local-images.sh           # one image at a time
helm install mmx deploy/helm/mmx --namespace mmx --create-namespace \
  -f deploy/helm/mmx/values-local.yaml
kubectl get pods --namespace mmx --watch
helm test mmx --namespace mmx

kubectl port-forward --namespace mmx service/mmx-web 3000:3000
kubectl port-forward --namespace mmx service/mmx-websocket 8090:8080   # 8080 is taken on the owner's Mac

container cluster delete --name mmx
```

Kustomize works too, with `kubectl apply -k deploy/kustomize/overlays/local`.

- **`--shared-image-store` can't be added later.** It also lets root on a node read every local image,
  and a local image beats a registry image of the same name, so keep it to development.
- **Rebuilt images need a restart.** `values-local.yaml` uses `pullPolicy: Always`. Run
  `kubectl rollout restart` on the Deployment or StatefulSet.
- **It doesn't resemble production timing.** Nothing is pinned, and the timeouts are 10 times longer.
- **Rust images are large,** so watch `df -h /`. Remove old ones with `container image delete`.
- `local-images.sh` builds from a copy under `/tmp`, because the builder is very slow when `target/`
  is under its context, even if excluded.

The context name and NetworkPolicy enforcement under the bundled kindnet CNI haven't been checked.
`kubectl config get-contexts` shows the real name.

## First real run

Run these once there's a machine free. Each one tests something static checks can't.

```sh
deploy/scripts/validate.sh

# 1. Images build, and the second run shows the cache working
PLATFORMS=linux/amd64 docker buildx bake -f deploy/docker/docker-bake.hcl core --load
touch services/api/rest/src/main.rs
PLATFORMS=linux/amd64 docker buildx bake -f deploy/docker/docker-bake.hcl core --load

# 2. The chart installs, and an order goes through
kind create cluster --name mmx --config deploy/kind/ci.yaml
for image in node matching-engine api mmx outbox websocket market-data trade-reporter; do
  kind load docker-image "mmx/$image:dev" --name mmx
done
helm install mmx deploy/helm/mmx --namespace mmx --create-namespace \
  -f deploy/helm/mmx/values-local.yaml --set image.pullPolicy=Never --set ui.enabled=false \
  --wait --timeout 10m
helm test mmx --namespace mmx --logs

# 3. Failover: an order still goes through with the leader's pod gone
kubectl delete pod mmx-node-0 --namespace mmx --grace-period=0 --force
helm test mmx --namespace mmx --logs
```

Then check these by hand:

- On prepared nodes, thread pinning takes effect: look for `pinned` in the `node` container's log, and
  read `/proc/<pid>/task/*/status` for `Cpus_allowed_list`.
- Pod QoS is `Guaranteed`: `kubectl get pod mmx-node-0 -o jsonpath='{.status.qosClass}'`.
- A pod from outside `networkPolicy.gameServers` can't reach a gateway.
- A restarted member rejoins on its new IP. This relies on Aeron re-resolving names.
- A member restarted in place gets past the mark files it left behind, and the node and engine recover
  if only one of them crashes.
- `docker buildx bake --print` accepts the Bake file, which hasn't been parsed by buildx yet.

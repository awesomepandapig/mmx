#!/bin/sh
# Static checks for deploy/. Needs helm, and kubectl or kustomize. Uses kubeconform if it's installed.
# Nothing here contacts a cluster or runs a container.
set -eu

cd "$(dirname "$0")/../.."
chart=deploy/helm/mmx
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
failed=0

pass() {
    echo "ok    $1"
}

fail() {
    echo "FAIL  $1"
    failed=1
}

# Fails unless file $2 contains text $3.
expect() {
    if grep -qF -- "$3" "$2"; then pass "$1"; else fail "$1"; fi
}

# Fails if file $2 contains text $3.
refuse() {
    if grep -qF -- "$3" "$2"; then fail "$1"; else pass "$1"; fi
}

# Like `expect` and `refuse`, for a name that must match a whole line, so mmx-web isn't mmx-websocket.
expect_line() {
    if grep -qE -- "^[[:space:]]*(- )?$3\$" "$2"; then pass "$1"; else fail "$1"; fi
}

refuse_line() {
    if grep -qE -- "^[[:space:]]*(- )?$3\$" "$2"; then fail "$1"; else pass "$1"; fi
}

# Fails unless the chart refuses these values.
rejects() {
    name=$1
    shift
    if helm template mmx "$chart" "$@" >/dev/null 2>&1; then fail "$name"; else pass "$name"; fi
}

kustomize_build() {
    if command -v kustomize >/dev/null 2>&1; then
        kustomize build "$1"
    else
        kubectl kustomize "$1"
    fi
}

for script in deploy/docker/mmx-lib.sh deploy/docker/mmx-entrypoint.sh deploy/docker/mmx-node.sh \
    deploy/scripts/local-images.sh; do
    if sh -n "$script"; then pass "syntax of $script"; else fail "syntax of $script"; fi
done

CLUSTER_NAME=mmx-node NODE_ID=2 MMX_LIB=deploy/docker/mmx-lib.sh sh deploy/docker/mmx-node.sh render \
    >"$out/node.properties"
expect "member 2 knows its ID" "$out/node.properties" "aeron.cluster.member.id=2"
expect "member 2 uses its own archive port" "$out/node.properties" \
    "aeron.archive.control.channel=aeron:udp?endpoint=mmx-node-2.mmx-node:8012"
expect "members are listed in full" "$out/node.properties" \
    "|2,mmx-node-2.mmx-node:20002,mmx-node-2.mmx-node:20012,mmx-node-2.mmx-node:20022,mmx-node-2.mmx-node:20032,mmx-node-2.mmx-node:8012"
expect "disk sync is off by default" "$out/node.properties" "aeron.archive.file.sync.level=0"

helm lint "$chart" >/dev/null && pass "helm lint" || fail "helm lint"
helm lint "$chart" -f "$chart/values-local.yaml" >/dev/null && pass "helm lint, local" \
    || fail "helm lint, local"

helm template mmx "$chart" >"$out/default.yaml"
helm template mmx "$chart" --show-only templates/gateway.yaml >"$out/gateway.yaml"
helm template mmx "$chart" --set websocket.enabled=false --show-only templates/console.yaml >"$out/console-no-ws.yaml"
helm template mmx "$chart" -f "$chart/values-local.yaml" >"$out/local.yaml"
helm template mmx "$chart" --set ui.enabled=true >"$out/marketing.yaml"
helm template mmx "$chart" --set ui.enabled=false --set demo.enabled=false >"$out/bare.yaml"
helm template mmx "$chart" --set console.oidc.enabled=true --set console.passwordLogin=false \
    --set console.oidc.issuer=https://id.example --set console.oidc.clientId=mmx \
    --set console.oidc.clientSecret.name=mmx-oidc --set "console.oidc.groups.admin={ops-admin}" \
    --set "console.oidc.groups.viewer={support,audit}" >"$out/oidc.yaml"
helm template mmx "$chart" --set demo.enabled=true --set demo.bots.enabled=true \
    --set webhookUrl=https://game.example/webhook \
    --set consumers.observer.enabled=true >"$out/full.yaml"
helm template mmx "$chart" --set questdbRebuild.enabled=true \
    --show-only templates/questdb-rebuild.yaml >"$out/rebuild.yaml"
helm template other "$chart" --namespace elsewhere >"$out/renamed.yaml"
helm lint "$chart" -f "$chart/values-performance.yaml" >/dev/null
helm template mmx "$chart" -f "$chart/values-performance.yaml" >"$out/performance.yaml"
expect "performance profile pins threads" "$out/performance.yaml" 'PIN_THREADS: "true"'
helm template mmx "$chart" --set image.digests.api=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa >"$out/digest.yaml"
expect "digest overrides image tag" "$out/digest.yaml" "api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
rejects "invalid image digest is refused" --set image.digests.api=sha256:bad
helm template mmx "$chart" --set gateway.service.type=LoadBalancer --set gateway.service.nodePort=30800 \
    --set gateway.service.loadBalancerIP=192.0.2.10 --set gateway.service.externalTrafficPolicy=Local \
    --set 'gateway.service.loadBalancerSourceRanges={192.0.2.0/24}' \
    --set console.service.type=NodePort --set console.service.nodePort=30993 \
    --set websocket.service.type=NodePort --set websocket.service.nodePort=30880 >"$out/exposure.yaml"
expect "gateway supports fixed NodePort" "$out/exposure.yaml" "nodePort: 30800"
expect "console supports fixed NodePort" "$out/exposure.yaml" "nodePort: 30993"
expect "websocket supports fixed NodePort" "$out/exposure.yaml" "nodePort: 30880"
expect "load balancer source ranges are rendered" "$out/exposure.yaml" "192.0.2.0/24"
expect "traffic policy is rendered" "$out/exposure.yaml" "externalTrafficPolicy: Local"
pass "helm template, all flag combinations"

expect "shared memory is memory-backed" "$out/default.yaml" "medium: Memory"
expect "members are spread across nodes" "$out/default.yaml" "requiredDuringSchedulingIgnoredDuringExecution"
expect "pod names stay stable before ready" "$out/default.yaml" "publishNotReadyAddresses: true"
expect "members start together" "$out/default.yaml" "podManagementPolicy: Parallel"
refuse "default shared CPUs are not pinned" "$out/default.yaml" 'PIN_THREADS: "true"'
expect "default voter storage is modest" "$out/default.yaml" "storage: 15Gi"
expect "voters require deliberate restarts" "$out/default.yaml" "type: OnDelete"
refuse_line "marketing web is off by default" "$out/default.yaml" "name: mmx-web"
expect_line "marketing web can be enabled separately" "$out/marketing.yaml" "name: mmx-web"
refuse "the demo is off by default" "$out/default.yaml" "mmx-demo"
refuse "no password is written down in values" deploy/helm/mmx/values.yaml "password: demo"
expect "turning on the demo generates its password" "$out/full.yaml" "DEMO_PASSWORD"
expect "the webhook URL can be seeded" "$out/full.yaml" "WEBHOOK_URL: \"https://game.example/webhook\""
expect_line "the console is in the chart by default" "$out/default.yaml" "name: mmx-console"
expect_line "the console remains when marketing is disabled" "$out/bare.yaml" "name: mmx-console"
expect "gateway readiness uses its application check" "$out/gateway.yaml" "path: /readyz"
expect "console monitors the websocket" "$out/default.yaml" "MMX_WEBSOCKET_URL"
refuse "disabled websocket is not monitored" "$out/console-no-ws.yaml" "MMX_WEBSOCKET_URL"
expect "local market-data has its actual resource override" "$out/local.yaml" 'cpu: "750m"'
expect "helm test has a real read-only hook" "$out/default.yaml" "helm.sh/hook: test"
refuse "helm test never submits an order" "$out/default.yaml" "POST /api/v1/orders"
refuse "no provider is connected by default" "$out/default.yaml" "MMX_OIDC_ISSUER"
expect "a provider can be connected at install" "$out/oidc.yaml" "MMX_OIDC_ISSUER"
expect "the client secret comes from a Secret" "$out/oidc.yaml" "name: mmx-oidc"
expect "groups reach each role" "$out/oidc.yaml" 'value: "support,audit"'
expect "passwords can start off" "$out/oidc.yaml" "MMX_PASSWORD_LOGIN"
rejects "a provider needs an admin group" --set console.oidc.enabled=true \
    --set console.oidc.issuer=https://id.example --set console.oidc.clientId=mmx \
    --set console.oidc.clientSecret.name=mmx-oidc
rejects "passwords can't be off with no provider" --set console.passwordLogin=false
expect "RBAC names the one ConfigMap" "$out/default.yaml" 'resourceNames: ["mmx-config"]'
expect "RBAC names the one Secret" "$out/default.yaml" 'resourceNames: ["mmx-control"]'
refuse "the console can't create or list anything" "$out/default.yaml" 'verbs: ["list"'
refuse "the chart makes no Ingress" "$out/default.yaml" "kind: Ingress"
expect "the admin token is generated" "$out/default.yaml" "MMX_ADMIN_TOKEN:"
expect "uninstalling keeps what operators made" "$out/default.yaml" "helm.sh/resource-policy: keep"
expect "every gateway can be found" "$out/default.yaml" "name: mmx-gateway-pods"
expect "gateways only mount the markets" "$out/default.yaml" "path: markets.yaml"
expect "outbox restart targets StatefulSet" "$out/default.yaml" 'value: "statefulset"'
expect "outbox restart permission is scoped" "$out/default.yaml" 'resourceNames: ["mmx-outbox"]'
expect "the console reaches the outbox" "$out/default.yaml" "MMX_OUTBOX_URL"
expect "only game servers reach gateways" "$out/default.yaml" "mmx.io/role: game-server"
refuse_line "the UI flag removes the UI" "$out/bare.yaml" "name: mmx-web"
refuse "the demo flag removes the demo" "$out/bare.yaml" "mmx-demo"
refuse "the local profile doesn't pin" "$out/local.yaml" 'PIN_THREADS: "true"'
expect "a consumer with storage is a StatefulSet" "$out/full.yaml" "name: mmx-outbox"
expect "release names reach member addresses" "$out/renamed.yaml" "other-mmx-node-0.other-mmx-node:20000"
expect "consumers get the tape in host:port form" "$out/default.yaml" \
    'value: "mmx-node-0.mmx-node:8010,mmx-node-1.mmx-node:8011,mmx-node-2.mmx-node:8012"'
expect "feeds keep their checkpoints" "$out/default.yaml" "/var/lib/mmx/checkpoints"
expect "reporter upgrades stop the old writer first" "$out/default.yaml" "type: Recreate"
expect_line "market data runs by default" "$out/default.yaml" "name: mmx-market-data"
refuse "an install never rebuilds QuestDB" "$out/full.yaml" "--rebuild"
expect "the rebuild job passes the flag" "$out/rebuild.yaml" "--rebuild"
expect "the rebuild job can't retry" "$out/rebuild.yaml" "backoffLimit: 0"
expect "the rebuild job's pod can't restart" "$out/rebuild.yaml" "restartPolicy: Never"
expect "the rebuild job's driver doesn't hold it open" "$out/rebuild.yaml" "restartPolicy: Always"

rejects "an even cluster is refused" --set cluster.replicas=4
rejects "fractional CPUs are refused when guaranteed" --set cluster.guaranteed=true --set cluster.engine.resources.cpu=1500m
rejects "pinning without exclusive CPUs is refused" --set cluster.guaranteed=false --set cluster.pinThreads=true
rejects "an unknown anti-affinity is refused" --set cluster.antiAffinity=sometimes
rejects "an unknown report source is refused" --set reportSource=3
rejects "multiple reporter writers are refused" --set consumers.trade-reporter.replicas=2

kustomize_build deploy/kustomize/base >"$out/base.yaml"
kustomize_build deploy/kustomize/overlays/local >"$out/overlay.yaml"
kustomize_build deploy/kustomize/jobs/questdb-rebuild >"$out/rebuild-job.yaml"
pass "kustomize, base, local overlay and rebuild job"

expect "base shared memory is memory-backed" "$out/base.yaml" "medium: Memory"
expect "base pins CPUs" "$out/base.yaml" "PIN_THREADS: \"true\""
refuse "the base never rebuilds QuestDB" "$out/base.yaml" "--rebuild"
refuse "the local overlay never rebuilds QuestDB" "$out/overlay.yaml" "--rebuild"
expect "the kustomize rebuild job can't retry" "$out/rebuild-job.yaml" "backoffLimit: 0"
expect "the overlay sets the local image" "$out/overlay.yaml" "image: mmx/matching-engine:dev"
expect_line "the overlay adds the UI" "$out/overlay.yaml" "name: mmx-web"
refuse "the overlay doesn't pin" "$out/overlay.yaml" 'PIN_THREADS: "true"'
refuse "the overlay has no production image left" "$out/overlay.yaml" "image: ghcr.io"

# The driver must reach every client, whichever kind of workload it is.
drivers=$(grep -c 'name: aeron-driver' "$out/base.yaml")
if [ "$drivers" -eq 3 ]; then pass "the driver reaches every client"; else fail "found $drivers drivers, wanted 3"; fi

# The two layouts must set the same node settings, or one of them has fallen behind.
sed -n 's/^\([A-Z_]*\)=.*/\1/p' deploy/kustomize/base/node.env | sort >"$out/kustomize-keys"
sed -n '/^kind: ConfigMap/,/^---/s/^  \([A-Z_]*\): .*/\1/p' "$out/default.yaml" | sort -u >"$out/helm-keys"
if comm -23 "$out/kustomize-keys" "$out/helm-keys" | grep .; then
    fail "the chart sets every setting in node.env"
else
    pass "the chart sets every setting in node.env"
fi

# Pods that write to /dev/shm must all use the same user, so their files are shared.
if grep -E '^\s+runAsUser:' "$out/default.yaml" | grep -qv 10001; then
    fail "every pod runs as the same user"
else
    pass "every pod runs as the same user"
fi

if grep -RInEi 'minecraft|hypixel' deploy .github/workflows/deploy.yml \
    --exclude=validate.sh >"$out/words"; then
    cat "$out/words"
    fail "generic language only"
else
    pass "generic language only"
fi

if command -v kubeconform >/dev/null 2>&1; then
    for file in default local full base overlay; do
        kubeconform -summary -ignore-missing-schemas "$out/$file.yaml" || failed=1
    done
else
    echo "skip  kubeconform isn't installed"
fi

exit "$failed"

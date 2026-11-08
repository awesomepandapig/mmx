# Deploy MMX

Use the published Helm chart and GHCR images for Kubernetes. Build images only when developing a source change.

- [Helm installation](https://docs.mmx.exchange/installation/helm)
- [Local Docker installation](https://docs.mmx.exchange/installation/local)
- [Source build](https://docs.mmx.exchange/installation/source)
- [Production operations](https://docs.mmx.exchange/guides/production)

## Kubernetes

```sh
helm upgrade --install mmx oci://ghcr.io/awesomepandapig/mmx/charts/mmx \
  --version 0.1.0 --namespace mmx --create-namespace --wait --timeout 10m
helm test mmx --namespace mmx --logs
kubectl get services --namespace mmx
```

These commands require the corresponding release artifacts to be published. The default voters request 3 CPUs and 9 GiB of memory in total, with a 15 GiB PVC per voter. All default workloads request 7.75 CPUs, 17.75 GiB of memory, and 61 GiB of PVC storage. Allow operating-system and Kubernetes headroom. Three schedulable hosts and a persistent-volume storage class are required.

Services default to `ClusterIP`: REST **8000**, console **8093**, WebSocket **8080**. Configure `gateway.service`, `console.service`, and `websocket.service` for your ingress, load balancer, or NodePort. Terminate TLS at your proxy. `networkPolicy.gameServers` selects trusted REST callers; it does not authenticate players.

For a source chart checkout, the optional `deploy/helm/mmx/values-performance.yaml` profile enables dedicated CPUs and thread pinning on prepared Linux nodes. Shared CPUs are the default; kubelet CPU-manager and Linux sysctl tuning are optional throughput work.

## Operator access

Install the native `mmx` CLI from the matching release, then configure its reachable service URLs:

```sh
export MMX_URL=https://console.exchange.example
export MMX_GATEWAY_URLS=https://orders.exchange.example
kubectl get secret mmx-control --namespace mmx \
  -o jsonpath='{.data.MMX_ADMIN_TOKEN}' | base64 -d
# Set the decoded value in your operator environment.
export MMX_ADMIN_TOKEN='<decoded admin token>'
mmx health
mmx status
kubectl logs --namespace mmx \
  -l app.kubernetes.io/instance=mmx,app.kubernetes.io/component=console \
  --prefix=true --tail=200 | grep 'Console: first-run admin password:'
```

Read `Console: first-run admin password:` and complete first sign-in. `mmx auth reset-password` requires the admin token and revokes console sessions. Keep that token in your operator secret manager.

Configure markets and settlement through the CLI:

```sh
mmx markets set IRON --tick-size 0.01 --min-size 1 --max-size 100000
mmx markets get IRON
mmx webhook rotate-secret --raw
# Configure the printed secret in your receiver.
export WEBHOOK_SECRET='<generated signing secret>'
mmx webhook set-url https://game.example/webhooks/mmx
mmx webhook restart
mmx webhook status
```

Configure the same signing secret in the receiver before restarting the outbox. The receiver must be reachable from the outbox pods.

## State and upgrades

The console owns `mmx-config` and `mmx-control` after the first install. Both survive upgrades and uninstall. Chart seeds do not overwrite console edits. Client-side Helm rendering cannot perform the chart's live-object lookup; decide ownership explicitly when using GitOps.

**Preserve quorum:** voters use `OnDelete`. Restart one at a time, confirm catch-up, and leave the leader last. Review replay compatibility before upgrading engine code; preserve archive/output volumes and verify changed replay behavior on a disposable copy.

**One recorder owns QuestDB.** Stop the normal recorder before a destructive rebuild and never run two writers against its checkpoint. A forced pod deletion does not fence a partitioned writer.

The Kustomize tree is maintained separately and does not include the complete Helm control-plane setup. Use the Helm chart for the documented installation.

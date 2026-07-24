# Deploy agentic-api on Kubernetes

The repository includes a Kustomize-compatible base in `deploy/kubernetes`. It runs two stateless gateway replicas
against managed PostgreSQL and an external OpenAI-compatible inference service. The Service is `ClusterIP` because the
gateway does not authenticate inbound callers yet. Put an authenticated application boundary in front of it before
allowing traffic from outside the cluster.

## Prepare the image and configuration

Build and publish the production image described in the [container guide](container.md). The base uses
`agentic-api:dev` so it also works with images loaded directly into a local cluster. Before deploying to a shared
cluster, replace that image with an immutable registry digest in an environment-specific Kustomize overlay such as
`deploy/kubernetes/overlays/production/kustomization.yaml`:

```yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
namespace: agentic-api
resources:
  - ../..
images:
  - name: agentic-api
    newName: registry.example.com/vllm/agentic-api
    digest: sha256:replace-with-the-published-digest
```

Change `LLM_API_BASE` and `CORS_ALLOWED_ORIGINS` in `deploy/kubernetes/configmap.yaml` or patch them in the same
overlay. Keep `SKIP_LLM_READY_CHECK=false` when the inference service exposes `/health`. The recurring upstream check
uses `OPENAI_API_KEY` as a bearer credential when configured. For a provider without `/health`, set
`SKIP_LLM_READY_CHECK=true`; `/ready` will continue checking PostgreSQL but will omit the upstream check. Add a small
Responses API request as an external monitor in that configuration.

Create the namespace and Secret before applying the workload:

```console
kubectl apply -f deploy/kubernetes/namespace.yaml
kubectl --namespace agentic-api create secret generic agentic-api \
  --from-literal=DATABASE_URL='postgresql://agentic-api:replace-me@postgres.example.com:5432/agentic_api?sslmode=require' \
  --from-literal=OPENAI_API_KEY='replace-me'
```

Omit `OPENAI_API_KEY` when the inference service does not require a fallback credential. The request's
`Authorization` or Anthropic-compatible `x-api-key` header still takes precedence where the protocol requires it.
`secret.example.yaml` documents the expected keys but is deliberately excluded from the Kustomize base. Do not add
real credentials to that file or commit a generated Secret. Kubernetes Secrets are only base64-encoded by default;
enable encryption at rest and restrict Secret access in the cluster.

## Deploy and inspect the gateway

Apply the base or the environment overlay:

```console
kubectl apply -k deploy/kubernetes
kubectl --namespace agentic-api rollout status deployment/agentic-api
kubectl --namespace agentic-api get pods,service,networkpolicy,poddisruptionbudget
```

ConfigMap and Secret values are injected as environment variables and do not update inside existing pods. After
changing either object without otherwise changing the pod template, run
`kubectl --namespace agentic-api rollout restart deployment/agentic-api` and wait for the rollout to finish.

The base defines:

- two gateway replicas with a rolling update that keeps the current replicas available;
- a `ClusterIP` Service on port `9000`;
- startup and liveness probes on `/health`, plus a readiness probe on `/ready`;
- a 30-second pod termination grace period for endpoint propagation and the gateway's bounded eight-second drain;
- CPU and memory requests and limits that should be tuned from observed traffic;
- a preferred hostname anti-affinity rule for the two gateway replicas;
- a non-root, read-only runtime with no Linux capabilities or mounted service-account token;
- a default-deny ingress NetworkPolicy; and
- a PodDisruptionBudget that retains one replica during voluntary disruption.

The startup probe allows up to eleven minutes. The Deployment bounds the upstream startup wait at five minutes,
leaving about six minutes for the database connection and embedded migrations before Kubernetes restarts the pod. The
process does not bind its port until both steps succeed. `/health` then reports process liveness without depending on
remote services. `/ready` runs bounded PostgreSQL and inference checks concurrently and removes a pod from Service
endpoints when either required dependency fails.

## Choose a migration policy

By default, every new pod applies the embedded SQLx migrations before starting the HTTP server. PostgreSQL migration
locking serializes concurrent migration attempts, and a failed migration keeps the pod out of service. Keep migrations
backward-compatible with the previous gateway version so a rolling update can run old and new replicas together.

For a supervisor-managed schema:

1. apply the repository migrations in a release job before updating the Deployment;
2. verify the target schema and any required compatibility upgrades;
3. set `AGENTIC_API_SCHEMA_READY=1` in the gateway ConfigMap; and
4. roll out the gateway only after the release job succeeds.

The runtime image intentionally contains no shell database client, and the gateway has no migration-only command, so
the base does not pretend to provide an in-image migration Job. Use a trusted database migration image or deployment
controller. Never set `AGENTIC_API_SCHEMA_READY` merely to bypass a failed migration.

## Put an authenticated edge in front

Do not use `OPENAI_API_KEY` as a caller password. It is an upstream inference credential. Exposing the gateway directly
would let anonymous callers spend that credential and read or write unscoped stored state.

The repository deliberately does not include a ready-to-apply Ingress while inbound caller authentication remains
deployment-specific. Create the Ingress in an environment overlay only after an identity-aware proxy or authenticated
application service protects every `/v1/*` HTTP and WebSocket route. For ingress-nginx, configure external
authentication and include settings equivalent to:

```yaml
metadata:
  annotations:
    nginx.ingress.kubernetes.io/auth-url: "https://auth.example.com/verify"
    nginx.ingress.kubernetes.io/proxy-buffering: "off"
    nginx.ingress.kubernetes.io/proxy-http-version: "1.1"
    nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
    nginx.ingress.kubernetes.io/proxy-send-timeout: "3600"
```

Use TLS and appropriate connection and request limits. The authentication boundary must consume and strip caller
`Authorization` and `x-api-key` headers before forwarding to the gateway; otherwise the gateway treats them as
upstream inference credentials instead of using its configured fallback. Use mTLS, a trusted non-forwarded identity
mechanism, or another platform control between the edge and gateway until inbound and upstream credentials are
separated in the gateway.

The base NetworkPolicy denies all pod-network ingress to the gateway. When the authenticated boundary is an
ingress-nginx controller, copy `network-policy-ingress.example.yaml` into the overlay with the authenticated Ingress.
Its selectors
admit only pods labeled `app.kubernetes.io/name=ingress-nginx` in the `ingress-nginx` namespace. Patch both selectors
when the controller uses different labels or a different namespace. Keep the default-deny policy in place, and verify
that the cluster's network plugin enforces NetworkPolicy before relying on it as a security boundary.

The portable base does not restrict egress because the DNS resolver, managed PostgreSQL addresses, ports, and external
inference destinations are environment-specific. Add a default-deny egress policy and explicit DNS, database, and
inference allowances in the production overlay when the cluster's network plugin can express those destinations.

SSE streams need buffering disabled from the client through every proxy. WebSocket clients should use `wss`, send
keepalive pings, and reconnect with backoff after a rollout or node disruption. Stored continuation state is in
PostgreSQL, so reconnects do not require session affinity; clients can continue with a conversation or previous
response ID through any ready replica.

Kubernetes begins endpoint removal at the same time as graceful pod termination. The five-second `preStop` delay gives
controllers time to observe the terminating endpoint before `SIGTERM` reaches the gateway. The gateway then stops
accepting new requests, drains active HTTP requests and WebSockets for up to eight seconds, and exits. The 30-second
grace period includes both phases. Verify the external load balancer's endpoint propagation and retry behavior.

Preferred anti-affinity encourages the scheduler to place replicas on different nodes without making a single-node
development cluster unschedulable. Use hard topology-spread constraints across nodes or zones when production
availability requires them. The PodDisruptionBudget only limits voluntary disruptions; it cannot prevent simultaneous
loss of co-located replicas during a node failure.

## Verify persistence and transport

Reach the private Service through a temporary port-forward in a separate terminal:

```console
kubectl --namespace agentic-api port-forward service/agentic-api 9000:9000
curl --fail http://127.0.0.1:9000/health
curl --fail http://127.0.0.1:9000/ready
```

Send a stored Responses request, save its response ID, restart the Deployment, and continue with
`previous_response_id`. A successful continuation after the rollout verifies that PostgreSQL, rather than a pod
filesystem, owns the state:

```console
first_response_id=$(
  curl --fail --silent --show-error http://127.0.0.1:9000/v1/responses \
    --header "Content-Type: application/json" \
    --data '{"model":"Qwen/Qwen3-30B-A3B-FP8","input":"Reply with READY","store":true}' |
    jq --exit-status --raw-output .id
)

kubectl --namespace agentic-api rollout restart deployment/agentic-api
kubectl --namespace agentic-api rollout status deployment/agentic-api
```

The port-forward exits when the selected pod terminates. Start the same `kubectl port-forward` command again after the
rollout, then continue the request:

```console
curl --fail --silent --show-error http://127.0.0.1:9000/v1/responses \
  --header "Content-Type: application/json" \
  --data "$(jq --null-input --arg id "$first_response_id" '{
    model: "Qwen/Qwen3-30B-A3B-FP8",
    input: "What word did you return?",
    previous_response_id: $id,
    store: true
  }')"
```

Repeat the check through the authenticated ingress for HTTP streaming and WebSockets before a production release.
Size PostgreSQL pools, gateway replicas, and inference capacity together; adding gateway replicas cannot compensate
for a saturated database or inference service.

{{- define "mmx.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 40 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 40 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/* The StatefulSet and its headless Service share this name, which mmx-lib.sh relies on. */}}
{{- define "mmx.clusterName" -}}
{{- printf "%s-node" (include "mmx.fullname" .) -}}
{{- end -}}

{{- define "mmx.selectorLabels" -}}
app.kubernetes.io/name: mmx
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{- define "mmx.labels" -}}
{{ include "mmx.selectorLabels" . }}
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
app.kubernetes.io/version: {{ .root.Chart.AppVersion | quote }}
helm.sh/chart: {{ printf "%s-%s" .root.Chart.Name .root.Chart.Version }}
{{- end -}}

{{/* Call with dict "root" $ "image" "api". */}}
{{- define "mmx.image" -}}
{{- printf "%s/%s:%s" .root.Values.image.registry .image (.root.Values.image.tag | default .root.Chart.AppVersion) -}}
{{- end -}}

{{/* Requests equal limits when guaranteed. Otherwise CPU can burst. Call with dict "resources" r "guaranteed" bool. */}}
{{- define "mmx.resources" -}}
requests:
  cpu: {{ .resources.cpu | quote }}
  memory: {{ .resources.memory | quote }}
limits:
  {{- if .guaranteed }}
  cpu: {{ .resources.cpu | quote }}
  {{- end }}
  memory: {{ .resources.memory | quote }}
{{- end -}}

{{- define "mmx.podSecurityContext" -}}
runAsNonRoot: true
runAsUser: 10001
runAsGroup: 10001
fsGroup: 10001
seccompProfile:
  type: RuntimeDefault
{{- end -}}

{{- define "mmx.containerSecurityContext" -}}
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
capabilities:
  drop: ["ALL"]
{{- end -}}

{{/* Member addresses. Short names resolve through the pod's search domain, in any namespace. */}}
{{- define "mmx.memberHost" -}}
{{- $name := include "mmx.clusterName" .root -}}
{{- printf "%s-%d.%s" $name (int .id) $name -}}
{{- end -}}

{{- define "mmx.ingressEndpoints" -}}
{{- $root := . -}}
{{- $list := list -}}
{{- range $i := until (int .Values.cluster.replicas) -}}
{{- $list = append $list (printf "%s:%d" (include "mmx.memberHost" (dict "root" $root "id" $i)) (add 20000 $i)) -}}
{{- end -}}
{{- join "," $list -}}
{{- end -}}

{{/* POD_IP must be declared before any variable that uses this. */}}
{{- define "mmx.reportChannel" -}}
{{- printf "aeron:udp?endpoint=$(POD_IP):0|control=%s:%d|session-id=1002" (include "mmx.memberHost" .) (add 40450 (int .id)) -}}
{{- end -}}

{{- define "mmx.reportChannels" -}}
{{- $root := . -}}
{{- $list := list -}}
{{- range $i := until (int .Values.cluster.replicas) -}}
{{- $list = append $list (include "mmx.reportChannel" (dict "root" $root "id" $i)) -}}
{{- end -}}
{{- join "," $list -}}
{{- end -}}

{{/* Bare host:port lists, in member order, as the tape crate expects. */}}
{{- define "mmx.memberEndpoints" -}}
{{- $root := .root -}}
{{- $base := .base -}}
{{- $list := list -}}
{{- range $i := until (int $root.Values.cluster.replicas) -}}
{{- $list = append $list (printf "%s:%d" (include "mmx.memberHost" (dict "root" $root "id" $i)) (add $base $i)) -}}
{{- end -}}
{{- join "," $list -}}
{{- end -}}

{{- define "mmx.clusterHosts" -}}
{{- $root := . -}}
{{- $list := list -}}
{{- range $i := until (int .Values.cluster.replicas) -}}
{{- $list = append $list (include "mmx.memberHost" (dict "root" $root "id" $i)) -}}
{{- end -}}
{{- join "," $list -}}
{{- end -}}

{{/* The first entries of a container's env, for anything that reads the cluster. */}}
{{- define "mmx.aeronClientEnv" -}}
- name: POD_IP
  valueFrom:
    fieldRef:
      fieldPath: status.podIP
- name: AERON_DIR
  value: /dev/shm/aeron
- name: CLIENT_HOST
  value: "$(POD_IP)"
- name: TAPE_ARCHIVES
  value: {{ include "mmx.memberEndpoints" (dict "root" . "base" 8010) | quote }}
- name: TAPE_REPORTS
  value: {{ include "mmx.memberEndpoints" (dict "root" . "base" 40450) | quote }}
- name: CLUSTER_HOSTS
  value: {{ include "mmx.clusterHosts" . | quote }}
- name: CLUSTER_NODES
  value: {{ .Values.cluster.replicas | quote }}
{{- if .Values.nats.enabled }}
- name: NATS_URL
  value: "nats://{{ include "mmx.fullname" . }}-nats:4222"
{{- end }}
{{- if .Values.questdb.enabled }}
- name: QUESTDB_CONF
  value: "http::addr={{ include "mmx.fullname" . }}-questdb:9000;"
- name: QUESTDB_URL
  value: "http://{{ include "mmx.fullname" . }}-questdb:9000"
{{- end }}
{{- if .Values.valkey.enabled }}
- name: VALKEY_URL
  value: "redis://{{ include "mmx.fullname" . }}-valkey:6379"
{{- end }}
{{- end -}}

{{- define "mmx.secretName" -}}
{{- if .Values.secrets.existingSecret -}}
{{- .Values.secrets.existingSecret -}}
{{- else -}}
{{- printf "%s-secrets" (include "mmx.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/* Optional, so a pod still starts when no secret exists. */}}
{{- define "mmx.secretEnvFrom" -}}
- secretRef:
    name: {{ include "mmx.secretName" . }}
    optional: true
{{- end -}}

{{/* Call with dict "root" $ and, for a Job's init container, "native" true. */}}
{{- define "mmx.driverContainer" -}}
{{- $root := .root -}}
- name: aeron-driver
  image: {{ include "mmx.image" (dict "root" $root "image" $root.Values.cluster.node.image) | quote }}
  imagePullPolicy: {{ $root.Values.image.pullPolicy }}
  {{- if .native }}
  restartPolicy: Always
  {{- end }}
  args: ["driver"]
  env:
    - name: AERON_DIR
      value: /dev/shm/aeron
    - name: THREADING
      value: {{ $root.Values.driver.threadingMode | quote }}
    - name: SOCKET_BUFFER
      value: {{ $root.Values.driver.socketBuffer | quote }}
    - name: JAVA_HEAP_MIN
      value: {{ $root.Values.driver.heapMax | quote }}
    - name: JAVA_HEAP_MAX
      value: {{ $root.Values.driver.heapMax | quote }}
  securityContext:
    {{- include "mmx.containerSecurityContext" $root | nindent 4 }}
  startupProbe:
    exec:
      command: ["test", "-f", "/dev/shm/aeron/cnc.dat"]
    periodSeconds: 1
    failureThreshold: 60
  resources:
    {{- include "mmx.resources" (dict "resources" $root.Values.driver.resources "guaranteed" false) | nindent 4 }}
  volumeMounts:
    - name: shm
      mountPath: /dev/shm
    - name: tmp
      mountPath: /tmp
{{- end -}}

{{/* Call with dict "sizeLimit" value. */}}
{{- define "mmx.aeronVolumes" -}}
- name: shm
  emptyDir:
    medium: Memory
    sizeLimit: {{ .sizeLimit }}
- name: tmp
  emptyDir:
    sizeLimit: 64Mi
{{- end -}}

{{/* A TCP check, or HTTP once a service serves a path. Call with dict "port" n "path" string. */}}
{{- define "mmx.readinessProbe" -}}
{{- if .path }}
httpGet:
  path: {{ .path }}
  port: {{ .port }}
{{- else }}
tcpSocket:
  port: {{ .port }}
{{- end }}
periodSeconds: 5
{{- end -}}

{{- define "mmx.validate" -}}
{{- $replicas := int .Values.cluster.replicas -}}
{{- if or (lt $replicas 1) (eq (mod $replicas 2) 0) -}}
{{- fail "cluster.replicas must be odd, so a quorum survives a partition" -}}
{{- end -}}
{{- if .Values.cluster.guaranteed -}}
{{- range $name, $container := dict "node" .Values.cluster.node "engine" .Values.cluster.engine -}}
{{- if not (regexMatch "^[0-9]+$" (toString $container.resources.cpu)) -}}
{{- fail (printf "cluster.%s.resources.cpu must be a whole number when cluster.guaranteed is on" $name) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if and .Values.cluster.pinThreads (not .Values.cluster.guaranteed) -}}
{{- fail "cluster.pinThreads needs cluster.guaranteed, or threads are pinned to CPUs other pods also use" -}}
{{- end -}}
{{- if not (has .Values.cluster.antiAffinity (list "required" "preferred")) -}}
{{- fail "cluster.antiAffinity must be required or preferred" -}}
{{- end -}}
{{- with .Values.consumers.outbox -}}
{{- if .enabled -}}
{{- if not (dig "env" "WEBHOOK_URL" "" .) -}}
{{- fail "consumers.outbox.env.WEBHOOK_URL is required when outbox is enabled" -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if and .Values.consumers.outbox.enabled (not .Values.secrets.existingSecret) (not (hasKey .Values.secrets.values "WEBHOOK_SECRET")) -}}
{{- fail "outbox needs WEBHOOK_SECRET, in secrets.values or secrets.existingSecret" -}}
{{- end -}}
{{- if ge (int .Values.reportSource) $replicas -}}
{{- fail "reportSource must be a cluster member" -}}
{{- end -}}
{{- end -}}

{{- define "steward.apiserverName" -}}steward-apiserver{{- end -}}
{{- define "steward.controllerName" -}}steward-controller{{- end -}}
{{- define "steward.mintName" -}}steward-mint{{- end -}}
{{- define "steward.webName" -}}steward-web{{- end -}}
{{- define "steward.triggerReceiverName" -}}steward-trigger-receiver{{- end -}}
{{- define "steward.image" -}}
{{- $repository := default .root.Values.images.repository .image.repository -}}
{{- $tag := default .root.Chart.AppVersion .image.tag -}}
{{- printf "%s:%s" $repository $tag -}}{{- with .image.digest -}}@{{ . }}{{- end -}}
{{- end -}}
{{- define "steward.mcpGwInternalVersion" -}}
{{- if eq .Values.connectionsBridge.mcpGatewayAuthorityContract "steward.connections.github/v1" -}}
0.3.2
{{- else if eq .Values.connectionsBridge.mcpGatewayAuthorityContract "steward.connections.github/v2" -}}
0.4.9
{{- else -}}
{{ .Values.connectionsBridge.mcpGatewayVersion }}
{{- end -}}
{{- end -}}

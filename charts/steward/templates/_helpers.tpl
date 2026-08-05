{{- define "steward.apiserverName" -}}steward-apiserver{{- end -}}
{{- define "steward.controllerName" -}}steward-controller{{- end -}}
{{- define "steward.mintName" -}}steward-mint{{- end -}}
{{- define "steward.image" -}}{{ .root.Values.images.repository }}:{{ .image.tag }}@{{ .image.digest }}{{- end -}}

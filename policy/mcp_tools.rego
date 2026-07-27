package steward.mcp

default allow := false

email_matches if {
	input.tokenClaims.email == input.principal
}

email_available if {
	is_string(input.tokenClaims.email)
}

principal_available if {
	is_string(input.principal)
}

subject_matches if {
	input.tokenClaims.sub == input.principal
}

principal_matches if {
	email_matches
	subject_matches
}

authority_claims_match if {
	claims := input.tokenClaims
	authority := claims.steward
	authority.acting_as == "user"
	authority.version == 1
	is_string(authority.runtime_uid)
	authority.runtime_uid != ""
}

tool_grant_matches if {
	authority := input.tokenClaims.steward
	some grant in authority.tools
	grant.provider == input.service
	grant.resource == input.tool
	grant.action == input.actionClass
}

allow if {
	principal_matches
	authority_claims_match
	tool_grant_matches
}

denial_reason := "verified token claims are unavailable" if {
	not is_object(input.tokenClaims)
} else := "verified token email is unavailable" if {
	not email_available
} else := "authenticated request principal is unavailable" if {
	not principal_available
} else := "verified token email does not match the request" if {
	not email_matches
} else := "verified token subject does not match the request" if {
	not subject_matches
} else := "verified token authority claims are invalid" if {
	not authority_claims_match
} else := "verified token has no matching tool grant"

decision := {"allow": true} if {
	allow
}

decision := {"allow": false, "reason": denial_reason} if {
	not allow
}

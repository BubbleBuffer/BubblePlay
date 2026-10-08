package main

import (
	"encoding/json"
	"regexp"
	"strings"
)

var approvalIDPattern = regexp.MustCompile(`^[A-Za-z0-9_-]{22,64}$`)
var browserVersionPattern = regexp.MustCompile(`^(?:Edg|EdgA|EdgiOS|Firefox|FxiOS|Chrome|CriOS|Version)/([0-9]{1,3})(?:[. /]|$)`)

func validApprovalID(id string) bool {
	return approvalIDPattern.MatchString(id)
}
func validApprovalAgent(agent string) bool {
	if len(agent) == 0 || len(agent) > 80 {
		return false
	}
	for _, c := range agent {
		if c < 32 || c > 126 {
			return false
		}
	}
	return true
}
func validApprovalDecision(m map[string]json.RawMessage) bool {
	return exact(m, "type", "id", "approve") && str(m, "type") == "approval-decision" && validApprovalID(str(m, "id")) && isBoolean(m, "approve")
}
func browserAgent(ua string) string {
	if len(ua) > 4096 {
		ua = ua[:4096]
	}
	browser := "Unknown browser"
	for _, candidate := range []struct{ token, name string }{
		{"Edg/", "Edge"}, {"EdgA/", "Edge"}, {"EdgiOS/", "Edge"},
		{"Firefox/", "Firefox"}, {"FxiOS/", "Firefox"},
		{"Chrome/", "Chrome"}, {"CriOS/", "Chrome"}, {"Version/", "Safari"},
	} {
		index := strings.Index(ua, candidate.token)
		if index < 0 || candidate.name == "Safari" && !strings.Contains(ua, "Safari/") {
			continue
		}
		browser = candidate.name
		if match := browserVersionPattern.FindStringSubmatch(ua[index:]); match != nil {
			browser += " " + match[1]
		}
		break
	}
	os := "unknown OS"
	switch {
	case strings.Contains(ua, "Windows"):
		os = "Windows"
	case strings.Contains(ua, "Android"):
		os = "Android"
	case strings.Contains(ua, "iPhone") || strings.Contains(ua, "iPad") || strings.Contains(ua, "iPod"):
		os = "iOS"
	case strings.Contains(ua, "CrOS"):
		os = "ChromeOS"
	case strings.Contains(ua, "Macintosh") || strings.Contains(ua, "Mac OS X"):
		os = "macOS"
	case strings.Contains(ua, "Linux"):
		os = "Linux"
	}
	return browser + " on " + os
}

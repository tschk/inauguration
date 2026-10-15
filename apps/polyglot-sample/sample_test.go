package main

import "testing"

func TestAnswer(t *testing.T) {
	if ans := answer(); ans != 42 {
		t.Errorf("Expected answer to return 42, got %d", ans)
	}
}

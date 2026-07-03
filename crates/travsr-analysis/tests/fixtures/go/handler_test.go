package handler

import (
	"testing"
)

func TestName(t *testing.T) {
	h := New()
	if h.Name() != "default" {
		t.Fatal("unexpected name")
	}
}

func TestNew(t *testing.T) {
	h := New()
	if h == nil {
		t.Fatal("New returned nil")
	}
}

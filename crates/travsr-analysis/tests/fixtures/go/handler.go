package handler

import (
	"net/http"
)

// Doer is a test interface.
type Doer interface {
	Do(req *http.Request) (*http.Response, error)
}

// Handler serves HTTP requests.
type Handler struct {
	name   string
	client Doer
}

// New constructs a Handler.
func New() *Handler {
	return &Handler{name: "default"}
}

// Name returns the handler name.
func (h Handler) Name() string {
	return h.name
}

// ServeHTTP implements http.Handler.
func (h Handler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	_ = w
	_ = r
}

// Handle processes a request using the Doer client (pointer receiver).
func (h *Handler) Handle(req *http.Request) (*http.Response, error) {
	return h.client.Do(req)
}

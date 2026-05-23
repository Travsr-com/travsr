// Package controller implements a simplified kubectl-style reconcile loop.
// Used as a benchmark fixture for the travsr Go Tree-sitter indexer (INDEX-170).
// Exercises all 7 node kinds: function, method, class(struct), interface,
// type(alias), var, import.
package controller

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"time"
)

// ── Constants and package-level vars ─────────────────────────────────────────

const (
	DefaultResyncPeriod   = 10 * time.Second
	DefaultWorkerCount    = 4
	DefaultMaxRetries     = 5
	DefaultQueueSize      = 1024
	AnnotationManagedBy   = "travsr.io/managed-by"
	AnnotationLastApplied = "travsr.io/last-applied"
	FinalizerName         = "travsr.io/finalizer"
)

var (
	ErrNotFound       = errors.New("resource not found")
	ErrAlreadyExists  = errors.New("resource already exists")
	ErrInvalidSpec    = errors.New("invalid resource spec")
	ErrReconcilePanic = errors.New("reconcile panicked")
)

// ── Type aliases ──────────────────────────────────────────────────────────────

type ResourceName = string
type NamespacedName = string
type Generation = int64
type ResourceVersion = string

// ── Interfaces ────────────────────────────────────────────────────────────────

// Reconciler is the core interface every controller must implement.
type Reconciler interface {
	Reconcile(ctx context.Context, req Request) (Result, error)
}

// Client provides read/write access to the Kubernetes API.
type Client interface {
	Get(ctx context.Context, key NamespacedName, obj Object) error
	List(ctx context.Context, list ObjectList, opts ...ListOption) error
	Create(ctx context.Context, obj Object) error
	Update(ctx context.Context, obj Object) error
	Delete(ctx context.Context, obj Object) error
	Patch(ctx context.Context, obj Object, patch Patch) error
	Status() StatusClient
}

// StatusClient updates only the status subresource.
type StatusClient interface {
	Update(ctx context.Context, obj Object) error
	Patch(ctx context.Context, obj Object, patch Patch) error
}

// EventRecorder records Kubernetes events.
type EventRecorder interface {
	Event(obj Object, eventType, reason, message string)
	Eventf(obj Object, eventType, reason, messageFmt string, args ...interface{})
}

// Object is the base interface for all Kubernetes resources.
type Object interface {
	GetName() string
	GetNamespace() string
	GetGeneration() Generation
	GetResourceVersion() ResourceVersion
	GetAnnotations() map[string]string
	SetAnnotations(map[string]string)
	GetFinalizers() []string
	SetFinalizers([]string)
}

// ObjectList is the base interface for list types.
type ObjectList interface {
	GetItems() []Object
}

// Patch represents a patch to apply.
type Patch interface {
	Type() string
	Data(obj Object) ([]byte, error)
}

// ListOption modifies a list request.
type ListOption interface {
	ApplyToList(*ListOptions)
}

// ── Structs ───────────────────────────────────────────────────────────────────

// Request is the reconcile request passed to Reconcile().
type Request struct {
	NamespacedName NamespacedName
}

// Result is the reconcile result.
type Result struct {
	Requeue      bool
	RequeueAfter time.Duration
}

// ListOptions holds options for listing resources.
type ListOptions struct {
	Namespace     string
	LabelSelector string
	Limit         int64
	Continue      string
}

func (o *ListOptions) ApplyToList(target *ListOptions) {
	if o.Namespace != "" {
		target.Namespace = o.Namespace
	}
	if o.LabelSelector != "" {
		target.LabelSelector = o.LabelSelector
	}
	if o.Limit > 0 {
		target.Limit = o.Limit
	}
}

// Controller manages the reconcile loop for a single resource type.
type Controller struct {
	name        string
	reconciler  Reconciler
	client      Client
	recorder    EventRecorder
	queue       chan Request
	workers     int
	maxRetries  int
	resyncEvery time.Duration
	mu          sync.RWMutex
	started     bool
	stopped     chan struct{}
	metrics     *ControllerMetrics
}

// ControllerMetrics tracks per-controller operational counters.
type ControllerMetrics struct {
	ReconcileTotal   int64
	ReconcileErrors  int64
	ReconcileRetries int64
	QueueDepth       int64
	mu               sync.Mutex
}

func (m *ControllerMetrics) IncTotal() {
	m.mu.Lock()
	m.ReconcileTotal++
	m.mu.Unlock()
}

func (m *ControllerMetrics) IncErrors() {
	m.mu.Lock()
	m.ReconcileErrors++
	m.mu.Unlock()
}

func (m *ControllerMetrics) IncRetries() {
	m.mu.Lock()
	m.ReconcileRetries++
	m.mu.Unlock()
}

func (m *ControllerMetrics) Snapshot() ControllerMetrics {
	m.mu.Lock()
	defer m.mu.Unlock()
	return ControllerMetrics{
		ReconcileTotal:   m.ReconcileTotal,
		ReconcileErrors:  m.ReconcileErrors,
		ReconcileRetries: m.ReconcileRetries,
		QueueDepth:       m.QueueDepth,
	}
}

// ── Constructor ───────────────────────────────────────────────────────────────

// New constructs a Controller with the given reconciler and client.
func New(name string, r Reconciler, c Client) *Controller {
	return &Controller{
		name:        name,
		reconciler:  r,
		client:      c,
		queue:       make(chan Request, DefaultQueueSize),
		workers:     DefaultWorkerCount,
		maxRetries:  DefaultMaxRetries,
		resyncEvery: DefaultResyncPeriod,
		stopped:     make(chan struct{}),
		metrics:     &ControllerMetrics{},
	}
}

// WithWorkers sets the worker concurrency.
func (c *Controller) WithWorkers(n int) *Controller {
	c.workers = n
	return c
}

// WithResync sets the resync period.
func (c *Controller) WithResync(d time.Duration) *Controller {
	c.resyncEvery = d
	return c
}

// WithRecorder attaches an event recorder.
func (c *Controller) WithRecorder(r EventRecorder) *Controller {
	c.recorder = r
	return c
}

// Start launches the worker goroutines and the resync ticker.
func (c *Controller) Start(ctx context.Context) error {
	c.mu.Lock()
	if c.started {
		c.mu.Unlock()
		return fmt.Errorf("controller %s already started", c.name)
	}
	c.started = true
	c.mu.Unlock()

	for i := 0; i < c.workers; i++ {
		go c.runWorker(ctx)
	}
	go c.resyncLoop(ctx)

	<-ctx.Done()
	close(c.stopped)
	return nil
}

// Enqueue adds a reconcile request to the work queue.
func (c *Controller) Enqueue(req Request) {
	select {
	case c.queue <- req:
		c.mu.Lock()
		c.metrics.QueueDepth++
		c.mu.Unlock()
	default:
		// Queue full — drop and rely on resync.
	}
}

// Metrics returns a snapshot of the controller counters.
func (c *Controller) Metrics() ControllerMetrics {
	return c.metrics.Snapshot()
}

// Name returns the controller name.
func (c *Controller) Name() string {
	return c.name
}

// ── Internal helpers ──────────────────────────────────────────────────────────

func (c *Controller) runWorker(ctx context.Context) {
	for {
		select {
		case req, ok := <-c.queue:
			if !ok {
				return
			}
			c.processItem(ctx, req, 0)
		case <-ctx.Done():
			return
		}
	}
}

func (c *Controller) processItem(ctx context.Context, req Request, attempt int) {
	c.metrics.IncTotal()

	result, err := safeReconcile(ctx, c.reconciler, req)
	if err != nil {
		c.metrics.IncErrors()
		if attempt < c.maxRetries {
			c.metrics.IncRetries()
			go func() {
				time.Sleep(backoff(attempt))
				c.processItem(ctx, req, attempt+1)
			}()
		}
		if c.recorder != nil {
			c.recorder.Eventf(nil, "Warning", "ReconcileError",
				"controller %s failed to reconcile %s: %v", c.name, req.NamespacedName, err)
		}
		return
	}
	if result.Requeue {
		delay := result.RequeueAfter
		if delay == 0 {
			delay = time.Millisecond * 100
		}
		go func() {
			time.Sleep(delay)
			c.Enqueue(req)
		}()
	}
}

func (c *Controller) resyncLoop(ctx context.Context) {
	ticker := time.NewTicker(c.resyncEvery)
	defer ticker.Stop()
	for {
		select {
		case <-ticker.C:
			// Resync: re-enqueue all known resources.
			// (Actual implementation would list from the informer cache.)
		case <-ctx.Done():
			return
		case <-c.stopped:
			return
		}
	}
}

// ── Package-level helpers ─────────────────────────────────────────────────────

func safeReconcile(ctx context.Context, r Reconciler, req Request) (res Result, err error) {
	defer func() {
		if p := recover(); p != nil {
			err = fmt.Errorf("%w: %v", ErrReconcilePanic, p)
		}
	}()
	return r.Reconcile(ctx, req)
}

func backoff(attempt int) time.Duration {
	d := time.Duration(1<<uint(attempt)) * 100 * time.Millisecond
	if d > 30*time.Second {
		d = 30 * time.Second
	}
	return d
}

func hasFinalizer(obj Object, name string) bool {
	for _, f := range obj.GetFinalizers() {
		if f == name {
			return true
		}
	}
	return false
}

func addFinalizer(obj Object, name string) {
	if !hasFinalizer(obj, name) {
		obj.SetFinalizers(append(obj.GetFinalizers(), name))
	}
}

func removeFinalizer(obj Object, name string) {
	finalizers := obj.GetFinalizers()
	updated := finalizers[:0]
	for _, f := range finalizers {
		if f != name {
			updated = append(updated, f)
		}
	}
	obj.SetFinalizers(updated)
}

func isBeingDeleted(obj Object) bool {
	return obj.GetAnnotations()[AnnotationManagedBy] == ""
}

func annotate(obj Object, key, value string) {
	ann := obj.GetAnnotations()
	if ann == nil {
		ann = make(map[string]string)
	}
	ann[key] = value
	obj.SetAnnotations(ann)
}

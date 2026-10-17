// Package stewardactivities shows the two Temporal activities needed to start
// and await a governed Steward run. Register Activities after constructing its
// token source and result sink in the worker process; neither dependency is a
// Workflow or Activity payload.
package stewardactivities

import (
	"archive/tar"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"go.temporal.io/sdk/activity"
)

type TaskTokenSource interface {
	// Token exchanges the worker's current credential for a fresh Steward Task
	// token. Implementations must not log either credential.
	Token(context.Context) (string, error)
}

type ResultSink interface {
	// Store consumes the output tar without putting it in Temporal history and
	// returns a non-secret reference suitable for an Activity result.
	Store(ctx context.Context, taskUID string, output io.Reader) (string, error)
}

type Activities struct {
	StewardURL string
	HTTP       *http.Client
	Tokens     TaskTokenSource
	Results    ResultSink
	PollEvery  time.Duration
}

type SubmitRunInput struct {
	Workflow       string `json:"workflow"`
	EnvelopeDigest string `json:"envelopeDigest,omitempty"`
	InputTar       []byte `json:"inputTar,omitempty"`
}

type SubmitRunResult struct {
	TaskUID string `json:"taskUid"`
}

type AwaitRunInput struct {
	TaskUID string `json:"taskUid"`
}

type AwaitRunResult struct {
	TaskUID       string `json:"taskUid"`
	Phase         string `json:"phase"`
	FailureReason string `json:"failureReason,omitempty"`
	ResultRef     string `json:"resultRef,omitempty"`
	NoOutput      bool   `json:"noOutput,omitempty"`
}

type taskStatus struct {
	TaskUID       string `json:"taskUid"`
	Phase         string `json:"phase"`
	FailureReason string `json:"failureReason,omitempty"`
}

func (a *Activities) SubmitRun(ctx context.Context, input SubmitRunInput) (SubmitRunResult, error) {
	if input.Workflow == "" || !strings.Contains(input.Workflow, "@") {
		return SubmitRunResult{}, errors.New("workflow must be a versioned name such as release-summary@1")
	}

	info := activity.GetInfo(ctx)
	idempotencyKey := strings.Join([]string{
		info.Namespace,
		info.WorkflowExecution.ID,
		info.WorkflowExecution.RunID,
		info.ActivityID,
	}, ":")

	requestBody, err := json.Marshal(struct {
		Workflow       string `json:"workflow"`
		EnvelopeDigest string `json:"envelopeDigest,omitempty"`
	}{input.Workflow, input.EnvelopeDigest})
	if err != nil {
		return SubmitRunResult{}, fmt.Errorf("encode run request: %w", err)
	}

	response, err := a.request(ctx, http.MethodPost, "/v1/tasks", "application/json", bytes.NewReader(requestBody), map[string]string{
		"Idempotency-Key": idempotencyKey,
	})
	if err != nil {
		return SubmitRunResult{}, err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK && response.StatusCode != http.StatusAccepted {
		return SubmitRunResult{}, unexpectedStatus("create run", response.StatusCode)
	}

	var created taskStatus
	if err := json.NewDecoder(io.LimitReader(response.Body, 1<<20)).Decode(&created); err != nil {
		return SubmitRunResult{}, fmt.Errorf("decode create response: %w", err)
	}
	if created.TaskUID == "" {
		return SubmitRunResult{}, errors.New("create response did not include taskUid")
	}

	inputTar := input.InputTar
	if len(inputTar) == 0 {
		inputTar, err = emptyTar()
		if err != nil {
			return SubmitRunResult{}, err
		}
	}
	if err := a.expect(ctx, http.MethodPut, taskPath(created.TaskUID, "/inputs"), "application/x-tar", bytes.NewReader(inputTar), http.StatusNoContent); err != nil {
		return SubmitRunResult{}, err
	}
	if err := a.expect(ctx, http.MethodPost, taskPath(created.TaskUID, "/execute"), "", nil, http.StatusAccepted); err != nil {
		return SubmitRunResult{}, err
	}

	return SubmitRunResult{TaskUID: created.TaskUID}, nil
}

func (a *Activities) AwaitRun(ctx context.Context, input AwaitRunInput) (AwaitRunResult, error) {
	if input.TaskUID == "" {
		return AwaitRunResult{}, errors.New("taskUid is required")
	}
	pollEvery := a.PollEvery
	if pollEvery <= 0 {
		pollEvery = 5 * time.Second
	}

	for {
		status, err := a.readStatus(ctx, input.TaskUID)
		if err != nil {
			return AwaitRunResult{}, err
		}
		activity.RecordHeartbeat(ctx, status.Phase)
		if terminal(status.Phase) {
			result, resultErr := a.readResult(ctx, status)
			cleanupErr := a.expect(ctx, http.MethodDelete, taskPath(input.TaskUID, ""), "", nil, http.StatusAccepted)
			return result, errors.Join(resultErr, cleanupErr)
		}

		timer := time.NewTimer(pollEvery)
		select {
		case <-ctx.Done():
			timer.Stop()
			return AwaitRunResult{}, ctx.Err()
		case <-timer.C:
		}
	}
}

func (a *Activities) readStatus(ctx context.Context, taskUID string) (taskStatus, error) {
	response, err := a.request(ctx, http.MethodGet, taskPath(taskUID, ""), "", nil, nil)
	if err != nil {
		return taskStatus{}, err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return taskStatus{}, unexpectedStatus("read run", response.StatusCode)
	}
	var status taskStatus
	if err := json.NewDecoder(io.LimitReader(response.Body, 1<<20)).Decode(&status); err != nil {
		return taskStatus{}, fmt.Errorf("decode run status: %w", err)
	}
	return status, nil
}

func (a *Activities) readResult(ctx context.Context, status taskStatus) (AwaitRunResult, error) {
	result := AwaitRunResult{
		TaskUID:       status.TaskUID,
		Phase:         status.Phase,
		FailureReason: status.FailureReason,
	}
	response, err := a.request(ctx, http.MethodGet, taskPath(status.TaskUID, "/outputs"), "", nil, nil)
	if err != nil {
		return result, err
	}
	defer response.Body.Close()
	switch response.StatusCode {
	case http.StatusOK:
		if a.Results == nil {
			return result, errors.New("result sink is required for a non-empty output archive")
		}
		result.ResultRef, err = a.Results.Store(ctx, status.TaskUID, response.Body)
		if err != nil {
			return result, fmt.Errorf("store run output: %w", err)
		}
	case http.StatusNoContent:
		result.NoOutput = true
	case http.StatusConflict:
		return result, errors.New("run became non-terminal before output download")
	default:
		return result, unexpectedStatus("read run output", response.StatusCode)
	}
	return result, nil
}

func (a *Activities) expect(ctx context.Context, method, path, contentType string, body io.Reader, expected int) error {
	response, err := a.request(ctx, method, path, contentType, body, nil)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if response.StatusCode != expected {
		return unexpectedStatus(method+" "+path, response.StatusCode)
	}
	return nil
}

func (a *Activities) request(ctx context.Context, method, path, contentType string, body io.Reader, headers map[string]string) (*http.Response, error) {
	if a.HTTP == nil || a.Tokens == nil {
		return nil, errors.New("HTTP client and Task token source are required")
	}
	token, err := a.Tokens.Token(ctx)
	if err != nil {
		return nil, fmt.Errorf("obtain Task token: %w", err)
	}
	defer func() { token = "" }()

	request, err := http.NewRequestWithContext(ctx, method, strings.TrimRight(a.StewardURL, "/")+path, body)
	if err != nil {
		return nil, fmt.Errorf("build Steward request: %w", err)
	}
	request.Header.Set("Authorization", "Bearer "+token)
	if contentType != "" {
		request.Header.Set("Content-Type", contentType)
	}
	for name, value := range headers {
		request.Header.Set(name, value)
	}
	client := *a.HTTP
	client.CheckRedirect = func(_ *http.Request, _ []*http.Request) error {
		return http.ErrUseLastResponse
	}
	response, err := client.Do(request)
	if err != nil {
		return nil, fmt.Errorf("call Steward: %w", err)
	}
	return response, nil
}

func emptyTar() ([]byte, error) {
	var buffer bytes.Buffer
	writer := tar.NewWriter(&buffer)
	if err := writer.Close(); err != nil {
		return nil, fmt.Errorf("create empty input tar: %w", err)
	}
	return buffer.Bytes(), nil
}

func taskPath(taskUID, suffix string) string {
	return "/v1/tasks/" + url.PathEscape(taskUID) + suffix
}

func terminal(phase string) bool {
	return phase == "succeeded" || phase == "failed" || phase == "cancelled"
}

func unexpectedStatus(operation string, status int) error {
	return fmt.Errorf("%s returned HTTP %d", operation, status)
}

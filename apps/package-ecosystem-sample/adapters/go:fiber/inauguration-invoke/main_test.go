package main

import (
	"bytes"
	"io"
	"os"
	"strings"
	"testing"
)

func TestMainOutput(t *testing.T) {
	// Keep track of the original stdout
	oldStdout := os.Stdout

	// Create a pipe to capture stdout
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatalf("Failed to create pipe: %v", err)
	}

	// Replace os.Stdout with our writer
	os.Stdout = w

	// Run the main function
	main()

	// Close the writer so we can read from the reader
	w.Close()

	// Restore original os.Stdout
	os.Stdout = oldStdout

	// Read the captured output
	var buf bytes.Buffer
	_, err = io.Copy(&buf, r)
	if err != nil {
		t.Fatalf("Failed to read from pipe: %v", err)
	}

	output := buf.String()

	// Verify the output
	expected := "fiber"
	if !strings.Contains(output, expected) {
		t.Errorf("Expected output to contain %q, but got %q", expected, output)
	}
}

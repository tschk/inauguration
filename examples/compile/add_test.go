package main

import "testing"

func TestAdd(t *testing.T) {
	tests := []struct {
		name string
		a    int
		b    int
		want int
	}{
		{"positive numbers", 1, 2, 3},
		{"negative numbers", -1, -2, -3},
		{"zeros", 0, 0, 0},
		{"mixed numbers", 10, -5, 5},
		{"forty and two", 40, 2, 42},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := add(tt.a, tt.b); got != tt.want {
				t.Errorf("add() = %v, want %v", got, tt.want)
			}
		})
	}
}

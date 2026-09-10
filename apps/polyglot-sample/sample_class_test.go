package main

import "testing"

func TestNewCalculator(t *testing.T) {
	calc := NewCalculator()
	if calc.value != 0 {
		t.Errorf("Expected Initialized Calculator value to be 0, got %d", calc.value)
	}
}

func TestCalculatorAdd(t *testing.T) {
	tests := []struct {
		name     string
		addend   int
		expected int
	}{
		{"positive number", 10, 10},
		{"negative number", -5, -5},
		{"zero", 0, 0},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			calc := NewCalculator()
			res := calc.Add(tt.addend)
			if res != tt.expected {
				t.Errorf("Expected Add to return %d, got %d", tt.expected, res)
			}
			if calc.value != tt.expected {
				t.Errorf("Expected value to be %d, got %d", tt.expected, calc.value)
			}
		})
	}
}

func TestCalculatorAddConsecutive(t *testing.T) {
	calc := NewCalculator()

	res := calc.Add(10)
	if res != 10 {
		t.Errorf("Expected Add to return 10, got %d", res)
	}

	res = calc.Add(20)
	if res != 30 {
		t.Errorf("Expected Add to return 30, got %d", res)
	}

	res = calc.Add(-5)
	if res != 25 {
		t.Errorf("Expected Add to return 25, got %d", res)
	}

	if calc.value != 25 {
		t.Errorf("Expected final value to be 25, got %d", calc.value)
	}
}

func TestCalculatorAnswer(t *testing.T) {
	calc := NewCalculator()
	if ans := calc.Answer(); ans != 42 {
		t.Errorf("Expected Answer to return 42, got %d", ans)
	}
}

func TestAnswer(t *testing.T) {
	if ans := answer(); ans != 42 {
		t.Errorf("Expected answer to return 42, got %d", ans)
	}
}

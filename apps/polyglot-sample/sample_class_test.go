package main

import "testing"

func TestNewCalculator(t *testing.T) {
	calc1 := NewCalculator()
	if calc1.value != 0 {
		t.Error("Expected Initialized Calculator value to be 0, got", calc1.value)
	}

	calc2 := NewCalculator()
	calc1.Add(5)
	if calc1.value != 5 {
		t.Error("Expected calc1 value to be 5, got", calc1.value)
	}
	if calc2.value != 0 {
		t.Error("Expected second Calculator value to remain 0, got", calc2.value)
	}
}

func TestNewCalculatorIndependence(t *testing.T) {
	calc1 := NewCalculator()
	calc2 := NewCalculator()

	calc1.Add(10)

	if calc1.value != 10 {
		t.Error("Expected calc1 value to be 10, got", calc1.value)
	}
	if calc2.value != 0 {
		t.Error("Expected calc2 value to remain 0, got", calc2.value)
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
		t.Error("Expected Add to return 10, got", res)
	}

	res = calc.Add(20)
	if res != 30 {
		t.Error("Expected Add to return 30, got", res)
	}

	res = calc.Add(-5)
	if res != 25 {
		t.Error("Expected Add to return 25, got", res)
	}

	if calc.value != 25 {
		t.Error("Expected final value to be 25, got", calc.value)
	}
}

func TestCalculatorAnswer(t *testing.T) {
	calc := NewCalculator()
	if ans := calc.Answer(); ans != 42 {
		t.Error("Expected Answer to return 42, got", ans)
	}
}

func TestAnswer(t *testing.T) {
	if ans := answer(); ans != 42 {
		t.Error("Expected answer to return 42, got", ans)
	}
}

package main

type Calculator struct {
	value int
}

func NewCalculator() Calculator {
	return Calculator{value: 0}
}

func (c *Calculator) Answer() int {
	return 42
}

func (c *Calculator) Add(x int) int {
	v := c.value
	v += x
	c.value = v
	return v
}

func answer() int {
	calc := NewCalculator()
	return calc.Answer()
}

func main() {}

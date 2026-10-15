dofile("sample.lua")

assert(answer() == 42, "answer() should return 42")

local counter = makeCounter(10)
assert(counter.value == 10, "initial value should be 10")

local newVal = counter:inc()
assert(newVal == 11, "inc() should return 11")
assert(counter.value == 11, "value should be updated to 11")

print("All tests passed!")

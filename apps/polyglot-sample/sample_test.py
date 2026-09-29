import unittest
import sample

class TestSample(unittest.TestCase):
    def test_answer(self):
        self.assertEqual(sample.answer(), 42)

if __name__ == '__main__':
    unittest.main()

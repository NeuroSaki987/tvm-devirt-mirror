import unittest
from recovery_census import compare

class CensusTests(unittest.TestCase):
    def test_missing_previously_complete_entry_is_not_a_pass(self):
        old={"per_entry":[{"entry":"0x1","complete":True}]}
        self.assertEqual(compare([],old),(["0x1"],[],["0x1"]))

    def test_failure_and_gain_keep_the_addresses(self):
        old={"per_entry":[{"entry":"0x1","complete":True},{"entry":"0x2","complete":False}]}
        rows=[{"entry":"0x1","complete":False},{"entry":"0x2","complete":True}]
        self.assertEqual(compare(rows,old),(["0x1"],["0x2"],[]))

if __name__=="__main__":
    unittest.main()

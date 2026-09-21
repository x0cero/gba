"""Keep the connected-map exception narrow enough to catch actual drift."""
import unittest
from regress3d import Frame, check_figures


class ConnectedMapFigures(unittest.TestCase):
    def check_case(self, *, cell=(5, -1), gamecell=(5, 0), connections=4,
                   player=True, void=False):
        frame = Frame(1)
        frame.mapsize = (10, 10)
        frame.connections = connections
        frame.figs = [dict(idx=0, cell=cell, gamecell=gamecell,
                           player=player, void=void)]
        return check_figures([frame])[0]

    def test_boundary_step_on_connected_floor_is_valid(self):
        self.assertFalse(self.check_case())

    def test_unconnected_edge_is_not_a_handoff(self):
        self.assertTrue(self.check_case(connections=0))

    def test_empty_floor_is_never_allowed(self):
        self.assertTrue(self.check_case(void=True))

    def test_npc_outside_map_is_not_a_player_step(self):
        self.assertTrue(self.check_case(player=False))

    def test_more_than_one_cell_outside_is_drift(self):
        self.assertTrue(self.check_case(cell=(5, -2)))

    def test_anchor_must_match_engine_position(self):
        self.assertTrue(self.check_case(gamecell=(5, 2)))


if __name__ == '__main__':
    unittest.main()

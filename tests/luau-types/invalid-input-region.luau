--!strict

local invalid_coordinate = ui.panel {
    input_region = { rectangles = { { x = "left" } } },
}

local invalid_dimension = ui.panel {
    input_region = { rectangles = { { width = false } } },
}

local invalid_entry = ui.panel {
    input_region = { rectangles = { false } },
}

return { invalid_coordinate, invalid_dimension, invalid_entry }

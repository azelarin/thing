--!strict

local card: ThingShadow = { color = '#000000', blur = 4 }
-- a misspelled field in a literal is a runtime error; reading one is caught here.
local _softness = card.softness
local _missing_color: ThingShadow = { blur = 4 }
local _numeric_color: ThingShadow = { color = 0x000000 }
local _text_blur: ThingShadow = { color = '#000000', blur = '4' }

return ui.panel {
    style = {
        box_shadow = function()
            return '#000000'
        end,
    },
    children = {
        ui.path {
            data = 'M0 0H1V1H0Z',
            view_box = { x = 0, y = 0, width = 1, height = 1 },
            shadow = { color = '#000000', offset_x = true },
        },
    },
}

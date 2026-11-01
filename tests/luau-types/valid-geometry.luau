--!strict

-- a popup sized to its widest entry.
local entries = { "open settings", "lock the screen", "log out of this session" }
local labels: { ThingWidget } = {}
local measured: { ThingGeometry } = {}
for index, entry in entries do
    labels[index] = ui.text { text = entry }
    measured[index] = ui.geometry(labels[index])
end

local menu = ui.popup {
    style = {
        padding = 12,
        width = function()
            local width = 120
            for _, geometry in measured do
                local content = geometry:content()
                if content then
                    width = math.max(width, content.width + 24)
                end
            end
            return width
        end,
    },
    children = { ui.column { children = labels } },
}

-- a badge in a shared overlay, placed under whichever button was clicked.
local badge_x = ui.state(0)
local badge_y = ui.state(0)
local overlay = ui.stack {
    style = { width = 600, height = 32 },
    children = {
        ui.text {
            text = "new",
            style = {
                translate_x = function() return badge_x:get() end,
                translate_y = function() return badge_y:get() end,
            },
        },
    },
}

local function anchored(label: string): ThingWidget
    local button: ThingWidget
    local measured: ThingGeometry
    button = ui.button {
        children = { ui.text { text = label } },
        on_click = function()
            local box = measured:layout()
            local point = box and ui.convert(button, 0, box.height, overlay)
            if point then
                badge_x:set(point.x)
                badge_y:set(point.y)
            end
        end,
    }
    measured = ui.geometry(button)
    return button
end

local first = anchored("mail")
local status = ui.geometry(first)

-- a window title that follows a clipped image's measured box.
local artwork = ui.image {
    source = "/tmp/cover.png",
    fit = "cover",
    style = {
        width = 64,
        height = 64,
        clip = {
            path = "M32 0 A32 32 0 1 1 32 64 A32 32 0 1 1 32 0 Z",
            view_box = { x = 0, y = 0, width = 64, height = 64 },
            fit = "cover",
            fill_rule = "nonzero",
        },
    },
}
local cover = ui.geometry(artwork)

return {
    ui.panel {
        style = { height = 32 },
        children = {
            ui.stack {
                children = {
                    ui.row { children = { first, anchored("chat"), ui.button { children = { menu } } } },
                    overlay,
                },
            },
            ui.text {
                text = function()
                    local surface: string? = status:surface()
                    local visual = status:visual()
                    local available: boolean = status:available()
                    return if available and visual and surface then `{visual.width}` else "unmeasured"
                end,
            },
        },
    },
    ui.window {
        key = "player",
        title = function(): string
            local layout = cover:layout()
            return if layout then `player {layout.width}x{layout.height}` else "player"
        end,
        size = { width = 320, height = 240 },
        children = { artwork },
    },
}

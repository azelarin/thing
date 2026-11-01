--!strict

local opened = ui.state("")

local function open(target: string, index: number)
    opened:set(`{index}: {target}`)
end

-- a list that mixes strings and span tables needs its type in strict mode.
local guide: { ThingTextSpan } = {
    "read ",
    { text = "the guide", bold = true, link = "https://example.org/guide" },
    " or ",
    { text = "the index", italic = true, color = "#89b4fa", link = "https://example.org/index" },
}

return ui.panel {
    children = {
        ui.text {
            spans = guide,
            on_link = open,
            max_lines = 2,
            overflow = "ellipsis",
            style = { width = 240, letter_spacing = 0.5 },
        },
        ui.text {
            spans = function(): { ThingTextSpan }
                return { { text = "all ", weight = 600 }, { text = "tables", underline = true } }
            end,
        },
        ui.text {
            markup = function() return `<b>{opened:get()}</b> &amp; <a href="mailto:a@example.org">mail</a>` end,
            on_link = open,
        },
    },
}

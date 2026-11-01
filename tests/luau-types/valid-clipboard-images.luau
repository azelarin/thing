--!strict

local clipboard = services.clipboard

local function describe(status: ThingClipboardImageStatus): string
    if status == "ready" then return "image copied" end
    if status == "loading" then return "reading image…" end
    if status == "unsupported" then return "this image format is not supported" end
    if status == "failed" then return clipboard.image_error:get() end
    return ""
end

return ui.panel {
    children = {
        ui.text {
            text = function()
                local formats: { string } = clipboard.formats:get()
                return `{#formats} formats · {describe(clipboard.image_status:get())}`
            end,
        },
        ui.button {
            enabled = function() return clipboard.image_status:get() == "available" end,
            on_click = function() clipboard.read_image() end,
            children = { ui.text { text = "show image" } },
        },
        ui.image {
            source = function() return clipboard.image:get() end,
            style = { width = 120, height = 80 },
        },
        ui.button {
            on_click = function()
                local image = clipboard.image:get()
                if image then clipboard.set_image(image) end
            end,
            children = { ui.text { text = "copy it back" } },
        },
    },
}

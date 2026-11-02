-- valid luau whose composite flag fails runtime validation, so the host must
-- keep the previous surfaces. the new opacity would change the frame if any of
-- this configuration were applied.
return require('./gallery')({ opacity = 0.25, composite = 'wrong' })

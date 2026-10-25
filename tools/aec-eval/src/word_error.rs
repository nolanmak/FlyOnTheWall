//! Deterministic word alignment for the approved public speech probe.

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Errors {
    pub reference_words: usize,
    pub matches: usize,
    pub substitutions: usize,
    pub deletions: usize,
    pub insertions: usize,
}

pub(crate) fn compare(reference: &str, hypothesis: &str) -> Errors {
    let words = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
    };
    let reference = words(reference);
    let hypothesis = words(hypothesis);
    let mut distance = vec![vec![0; hypothesis.len() + 1]; reference.len() + 1];
    for (i, row) in distance.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, value) in distance[0].iter_mut().enumerate() {
        *value = j;
    }
    for i in 1..=reference.len() {
        for j in 1..=hypothesis.len() {
            distance[i][j] = (distance[i - 1][j - 1]
                + usize::from(reference[i - 1] != hypothesis[j - 1]))
            .min(distance[i - 1][j] + 1)
            .min(distance[i][j - 1] + 1);
        }
    }
    let mut result = Errors {
        reference_words: reference.len(),
        matches: 0,
        substitutions: 0,
        deletions: 0,
        insertions: 0,
    };
    let (mut i, mut j) = (reference.len(), hypothesis.len());
    // Equal-cost alignments prefer diagonal, then deletion, then insertion.
    while i > 0 || j > 0 {
        if i > 0
            && j > 0
            && distance[i][j]
                == distance[i - 1][j - 1] + usize::from(reference[i - 1] != hypothesis[j - 1])
        {
            if reference[i - 1] == hypothesis[j - 1] {
                result.matches += 1;
            } else {
                result.substitutions += 1;
            }
            i -= 1;
            j -= 1;
        } else if i > 0 && distance[i][j] == distance[i - 1][j] + 1 {
            result.deletions += 1;
            i -= 1;
        } else {
            result.insertions += 1;
            j -= 1;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn word_alignment_counts_substitutions_deletions_insertions_and_total_speech_loss() {
        assert_eq!(
            compare(
                "ask what your country can do",
                "ask what our country can really do"
            ),
            Errors {
                reference_words: 6,
                matches: 5,
                substitutions: 1,
                deletions: 0,
                insertions: 1
            }
        );
        assert_eq!(
            compare("ask what your country can do", "ask what country can do"),
            Errors {
                reference_words: 6,
                matches: 5,
                substitutions: 0,
                deletions: 1,
                insertions: 0
            }
        );
        assert_eq!(
            compare("ask what your country can do", ""),
            Errors {
                reference_words: 6,
                matches: 0,
                substitutions: 0,
                deletions: 6,
                insertions: 0
            }
        );
    }
    #[test]
    fn punctuation_and_case_do_not_count_as_speech_loss_but_extra_words_do() {
        assert_eq!(
            compare(
                "And so, my fellow Americans!",
                "and so my fellow americans."
            ),
            Errors {
                reference_words: 5,
                matches: 5,
                substitutions: 0,
                deletions: 0,
                insertions: 0
            }
        );
        assert_eq!(
            compare("country", "country country"),
            Errors {
                reference_words: 1,
                matches: 1,
                substitutions: 0,
                deletions: 0,
                insertions: 1
            }
        );
    }
}

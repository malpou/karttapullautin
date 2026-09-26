use std::str::FromStr;

#[derive(Debug, PartialEq)]
pub enum Operator {
    Equal,
    NotEqual,
}

/// A condition that is used to filter the records in the vectorconf file
#[derive(Debug, PartialEq)]
pub struct Condition {
    pub operator: Operator,
    pub key: String,
    pub value: String,
}

/// Each mapping represents one rule (line) of a vector mapping file:
/// `category|symbol code[T]|conditions`, e.g. `road|502T|highway=primary&bridge=yes`.
#[derive(Debug, PartialEq)]
pub struct Mapping {
    /// Free-text name (first field), exported as the GeoJSON `category`
    pub category: String,
    /// ISOM 2017-2 symbol code the shape is drawn with (second field, without the `T` suffix)
    pub symbol: String,
    /// A `T` suffix on the code: the feature passes over the others (bridge, upper level)
    /// and is drawn above them
    pub upper_level: bool,
    /// The conditions that must be met for this mapping to be applied
    pub conditions: Vec<Condition>,
}

impl FromStr for Mapping {
    type Err = String;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let row_data: Vec<&str> = line.trim().split('|').collect();
        if row_data.len() != 3 {
            return Err(format!(
                "vectorconf line does not contain 3 sections separated by '|', it has {}: {}",
                row_data.len(),
                line
            ));
        }
        let category = row_data[0].trim().to_string();
        let code = row_data[1].trim();
        let (symbol, upper_level) = match code.strip_suffix('T') {
            Some(symbol) => (symbol.to_string(), true),
            None => (code.to_string(), false),
        };
        if symbol.is_empty() {
            return Err(format!("Symbol code must not be empty: {line}"));
        }

        let conditions: Vec<Condition> = row_data[2]
            .split('&')
            .map(|param| {
                let (operator, d): (Operator, Vec<&str>) = if param.contains("!=") {
                    (Operator::NotEqual, param.splitn(2, "!=").collect())
                } else if param.contains("=") {
                    (Operator::Equal, param.splitn(2, "=").collect())
                } else {
                    return Err(format!(
                        "Condition does not contain a valid operator: {param}"
                    ));
                };
                Ok(Condition {
                    operator,
                    key: d[0].trim().to_string(),
                    value: d[1].trim().to_string(),
                })
            })
            .collect::<Result<Vec<_>, Self::Err>>()?;

        Ok(Self {
            category,
            symbol,
            upper_level,
            conditions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_mapping_from_str_invalid_no_symbol() {
        let line = "description||key1=value1";
        let mapping = Mapping::from_str(line);
        assert!(mapping.is_err());
    }

    #[test]
    fn test_mapping_from_str_invalid_no_conditions() {
        let line = "description|306|";
        let mapping = Mapping::from_str(line);
        assert!(mapping.is_err());
    }

    #[test]
    fn test_mapping_from_str_valid_single() {
        let line = "description|306|key1=value1";
        let mapping = Mapping::from_str(line).unwrap();
        let expected = Mapping {
            category: "description".to_string(),
            symbol: "306".to_string(),
            upper_level: false,
            conditions: vec![Condition {
                operator: Operator::Equal,
                key: "key1".to_string(),
                value: "value1".to_string(),
            }],
        };
        assert_eq!(mapping, expected);
    }

    #[test]
    fn test_mapping_from_str_valid_two() {
        let line = "description|306|key1=value1&key2!=value2";
        let mapping = Mapping::from_str(line).unwrap();
        let expected = Mapping {
            category: "description".to_string(),
            symbol: "306".to_string(),
            upper_level: false,
            conditions: vec![
                Condition {
                    operator: Operator::Equal,
                    key: "key1".to_string(),
                    value: "value1".to_string(),
                },
                Condition {
                    operator: Operator::NotEqual,
                    key: "key2".to_string(),
                    value: "value2".to_string(),
                },
            ],
        };
        assert_eq!(mapping, expected);
    }

    #[test]
    fn test_mapping_from_str_valid_more() {
        let line = "description|306|key1=value1&key2!=value2&key3=value3";
        let mapping = Mapping::from_str(line).unwrap();
        let expected = Mapping {
            category: "description".to_string(),
            symbol: "306".to_string(),
            upper_level: false,
            conditions: vec![
                Condition {
                    operator: Operator::Equal,
                    key: "key1".to_string(),
                    value: "value1".to_string(),
                },
                Condition {
                    operator: Operator::NotEqual,
                    key: "key2".to_string(),
                    value: "value2".to_string(),
                },
                Condition {
                    operator: Operator::Equal,
                    key: "key3".to_string(),
                    value: "value3".to_string(),
                },
            ],
        };
        assert_eq!(mapping, expected);
    }

    #[test]
    fn test_mapping_from_str_upper_level_suffix() {
        let mapping = Mapping::from_str("road|502T|highway=primary&bridge=yes").unwrap();
        assert_eq!(mapping.category, "road");
        assert_eq!(mapping.symbol, "502");
        assert!(mapping.upper_level);

        let mapping = Mapping::from_str(" road | 502 |highway=primary").unwrap();
        assert_eq!(mapping.category, "road");
        assert_eq!(mapping.symbol, "502");
        assert!(!mapping.upper_level);

        assert!(Mapping::from_str("road|T|highway=primary").is_err());
    }

    #[test]
    fn test_mapping_from_str_invalid() {
        let line = "306|key1=value1&key2!=value2";
        let result = Mapping::from_str(line);
        assert!(result.is_err());
    }

    #[test]
    fn test_mapping_from_str_invalid_operator() {
        let line = "description|306|key1=value1&key2>value2";
        let result = Mapping::from_str(line);
        assert!(result.is_err());
    }

    #[test]
    fn test_mapping_from_str_missing_sections() {
        let line = "306|key1=value1";
        let result = Mapping::from_str(line);
        assert!(result.is_err());
    }

    #[test]
    fn test_mapping_from_str_empty_line() {
        let line = "";
        let result = Mapping::from_str(line);
        assert!(result.is_err());
    }

    #[test]
    fn test_mapping_from_str_extra_sections() {
        let line = "description|306|key1=value1&key2!=value2|extra";
        let result = Mapping::from_str(line);
        assert!(result.is_err());
    }

    /// Make sure the bundled osm.txt file can be parsed
    #[test]
    fn test_mapping_from_str_osm() {
        let lines = include_str!("../../osm.txt");
        for line in lines.lines() {
            Mapping::from_str(line).unwrap();
        }
    }

    /// Make sure the bundled fastighetskartan.txt file can be parsed
    #[test]
    fn test_mapping_from_str_fastighetskartan() {
        let lines = include_str!("../../fastighetskartan.txt");
        for line in lines.lines() {
            Mapping::from_str(line).unwrap();
        }
    }
}
